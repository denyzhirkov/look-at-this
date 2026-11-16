//! Use cases: what an agent can ask for, checked against the access policy.

use std::fmt;
use std::sync::{Arc, Mutex};

use crate::domain::policy::{Access, AccessPolicy};
use crate::domain::snapshot::{RefError, Snapshot, one_line};
use crate::domain::viewport::{Viewport, clamp_to_window, is_thumbnail};
use crate::ports::{Rect, ScreenCapture, UiTree, WindowInfo, WindowSource};

/// Upper bound on accessibility elements read per observe.
const MAX_TREE_NODES: usize = 1500;
/// A zoomed screenshot may use the display's full Retina resolution.
const ZOOM_MAX_SCALE: f64 = 2.0;

#[derive(Debug, PartialEq)]
pub enum Shot {
    None,
    Window,
    /// Only the element named by `reference`, magnified.
    Region,
}

pub struct ObserveRequest {
    pub window_id: u32,
    pub screenshot: Shot,
    pub tree: bool,
    pub reference: Option<String>,
    pub max_edge: u32,
}

pub struct Observation {
    pub text: String,
    pub png: Option<Vec<u8>>,
}

#[derive(Debug, PartialEq)]
pub enum Error {
    WindowNotFound(u32),
    NotAllowed(String),
    /// The app has no bundle id, so it cannot be allowlisted at all.
    Unidentified(String),
    Forbidden(String),
    NothingRequested,
    RefRequired,
    StaleRef(String),
    BadRef(String),
    RefNotVisible(String),
    Thumbnail,
    System(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WindowNotFound(id) => {
                write!(f, "window {id} is not open; call list_windows for current ids")
            }
            Self::NotAllowed(app) => write!(
                f,
                "{app} is not on the allowlist. Ask the user to allow it by running \
                 `look-at-this allow {app}` themselves; do not run it for them"
            ),
            Self::Unidentified(app) => write!(
                f,
                "{app} has no bundle id, so it cannot be allowed, seen or controlled"
            ),
            Self::Forbidden(app) => write!(f, "{app} can never be seen or controlled"),
            Self::NothingRequested => write!(f, "nothing to return: enable `tree` or `screenshot`"),
            Self::RefRequired => write!(f, "screenshot \"region\" needs `ref`, an element like e12"),
            Self::StaleRef(r) => write!(
                f,
                "{r} is not from the latest observe of this window; call observe again"
            ),
            Self::BadRef(r) => write!(f, "{r} is not an element ref; refs look like e12"),
            Self::RefNotVisible(r) => write!(f, "{r} has no visible area inside the window"),
            Self::Thumbnail => write!(
                f,
                "this window is only a Stage Manager thumbnail because its app is in the \
                 background, so it cannot be observed. Ask the user to bring the app to the \
                 front or to turn Stage Manager off"
            ),
            Self::System(message) => write!(f, "{message}"),
        }
    }
}

pub struct Session {
    windows: Arc<dyn WindowSource>,
    capture: Arc<dyn ScreenCapture>,
    tree: Arc<dyn UiTree>,
    state: Mutex<State>,
}

struct State {
    snapshot: Option<Snapshot>,
    next_element_id: u64,
}

impl Session {
    /// `first_element_id` should differ between server runs, so that a ref an
    /// agent kept from a previous run is unlikely to name a new element.
    pub fn new(
        windows: Arc<dyn WindowSource>,
        capture: Arc<dyn ScreenCapture>,
        tree: Arc<dyn UiTree>,
        first_element_id: u64,
    ) -> Self {
        let state = Mutex::new(State { snapshot: None, next_element_id: first_element_id });
        Self { windows, capture, tree, state }
    }

    /// Windows of allowed apps only.
    pub fn list_windows(&self, policy: &AccessPolicy) -> Result<Vec<WindowInfo>, Error> {
        let mut windows = self.windows.windows().map_err(Error::System)?;
        windows.retain(|w| access(policy, w) == Access::Allowed);
        Ok(windows)
    }

    pub fn observe(&self, policy: &AccessPolicy, request: ObserveRequest) -> Result<Observation, Error> {
        let window = self.allowed_window(policy, request.window_id)?;
        if request.screenshot == Shot::Region {
            return self.zoom(&window, &request);
        }
        if !request.tree && request.screenshot == Shot::None {
            return Err(Error::NothingRequested);
        }

        let whole = Rect { x: 0.0, y: 0.0, width: window.frame.width, height: window.frame.height };
        let viewport = Viewport::fit(whole, 1.0, request.max_edge);
        let (width, height) = viewport.image_size();
        let mut text = format!(
            "window {} \"{}\" ({}), {width}x{height} px\n",
            window.id,
            one_line(&window.title),
            one_line(&window.app_name)
        );

        if request.tree {
            let tree = self
                .tree
                .tree(window.pid, window.id, MAX_TREE_NODES)
                .map_err(Error::System)?;
            if tree.root.frame.is_some_and(|actual| is_thumbnail(window.frame, actual)) {
                return Err(Error::Thumbnail);
            }
            let mut state = self.state.lock().unwrap();
            let origin = (window.frame.x, window.frame.y);
            let snapshot = Snapshot::build(window.id, origin, tree.root, state.next_element_id);
            state.next_element_id += snapshot.elements.len() as u64;
            text.push_str(&snapshot.render(&viewport));
            if tree.truncated {
                text.push_str("(tree incomplete: some elements were skipped because of the size or time limit, or could not be read)\n");
            }
            state.snapshot = Some(snapshot);
        }

        let png = match request.screenshot {
            Shot::Window => Some(
                self.capture
                    .capture(window.id, window.pid, whole, width, height)
                    .map_err(Error::System)?,
            ),
            _ => None,
        };
        Ok(Observation { text, png })
    }

    /// A magnified screenshot of one element. Leaves the snapshot untouched.
    fn zoom(&self, window: &WindowInfo, request: &ObserveRequest) -> Result<Observation, Error> {
        let reference = request.reference.as_deref().ok_or(Error::RefRequired)?;
        let frame = {
            let state = self.state.lock().unwrap();
            let snapshot = state
                .snapshot
                .as_ref()
                .ok_or_else(|| Error::StaleRef(reference.to_string()))?;
            snapshot.frame_of(window.id, reference).map_err(|e| match e {
                RefError::Malformed => Error::BadRef(reference.to_string()),
                RefError::Stale => Error::StaleRef(reference.to_string()),
                RefError::NoFrame => Error::RefNotVisible(reference.to_string()),
            })?
        };
        let region = clamp_to_window(frame, window.frame.width, window.frame.height)
            .ok_or_else(|| Error::RefNotVisible(reference.to_string()))?;
        let viewport = Viewport::fit(region, ZOOM_MAX_SCALE, request.max_edge);
        let (width, height) = viewport.image_size();
        let png = self
            .capture
            .capture(window.id, window.pid, region, width, height)
            .map_err(Error::System)?;
        Ok(Observation {
            text: format!(
                "zoomed view of {reference}, {width}x{height} px. \
                 Its pixels are not window coordinates.\n"
            ),
            png: Some(png),
        })
    }

    fn allowed_window(&self, policy: &AccessPolicy, window_id: u32) -> Result<WindowInfo, Error> {
        let window = self
            .windows
            .windows()
            .map_err(Error::System)?
            .into_iter()
            .find(|w| w.id == window_id)
            .ok_or(Error::WindowNotFound(window_id))?;
        let Some(bundle_id) = window.bundle_id.clone() else {
            return Err(Error::Unidentified(one_line(&window.app_name)));
        };
        match policy.check(&bundle_id) {
            Access::Allowed => Ok(window),
            Access::NotAllowed => Err(Error::NotAllowed(bundle_id)),
            Access::Forbidden => Err(Error::Forbidden(bundle_id)),
        }
    }
}

/// An app without a bundle id cannot be allowlisted, so it is never allowed.
fn access(policy: &AccessPolicy, window: &WindowInfo) -> Access {
    window.bundle_id.as_deref().map_or(Access::NotAllowed, |id| policy.check(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::{PortResult, UiNode, UiTreeSnapshot};

    /// Records every capture so tests can assert what was (not) captured.
    #[derive(Default)]
    struct Fake {
        captures: Mutex<Vec<(u32, Rect, u32, u32)>>,
        tree_reads: Mutex<u32>,
        /// Size accessibility reports for the window; `None` reports no frame.
        ax_window_size: Option<(f64, f64)>,
    }

    impl WindowSource for Fake {
        fn windows(&self) -> PortResult<Vec<WindowInfo>> {
            let window = |id, bundle: &str, x| WindowInfo {
                id,
                pid: id as i32,
                bundle_id: Some(bundle.to_string()),
                app_name: bundle.to_string(),
                title: "t".to_string(),
                frame: Rect { x, y: 50.0, width: 2000.0, height: 1000.0 },
            };
            let mut anonymous = window(4, "", 0.0);
            anonymous.bundle_id = None;
            anonymous.app_name = "helper".to_string();
            Ok(vec![
                window(1, "com.apple.TextEdit", 100.0),
                window(2, "com.apple.Notes", 0.0),
                window(3, "com.apple.Terminal", 0.0),
                anonymous,
            ])
        }
    }

    impl ScreenCapture for Fake {
        fn capture(&self, id: u32, pid: i32, region: Rect, w: u32, h: u32) -> PortResult<Vec<u8>> {
            assert_eq!(pid, id as i32, "capture must name the process that was access-checked");
            self.captures.lock().unwrap().push((id, region, w, h));
            Ok(vec![1, 2, 3])
        }
    }

    impl UiTree for Fake {
        fn tree(&self, _pid: i32, _window_id: u32, _max: usize) -> PortResult<UiTreeSnapshot> {
            *self.tree_reads.lock().unwrap() += 1;
            let button = UiNode {
                role: "AXButton".to_string(),
                title: "Save".to_string(),
                // Screen coordinates: 200 pt right and 100 pt below the window origin.
                frame: Some(Rect { x: 300.0, y: 150.0, width: 100.0, height: 40.0 }),
                enabled: true,
                ..UiNode::default()
            };
            let root = UiNode {
                role: "AXWindow".to_string(),
                frame: self
                    .ax_window_size
                    .map(|(width, height)| Rect { x: 100.0, y: 50.0, width, height }),
                enabled: true,
                children: vec![button],
                ..UiNode::default()
            };
            Ok(UiTreeSnapshot { root, truncated: false })
        }
    }

    fn session() -> (Session, Arc<Fake>) {
        session_with(Fake::default())
    }

    fn session_with(fake: Fake) -> (Session, Arc<Fake>) {
        let fake = Arc::new(fake);
        (Session::new(fake.clone(), fake.clone(), fake.clone(), 1), fake)
    }

    /// Terminal is allowlisted on purpose: the hard denylist must still win.
    fn policy() -> AccessPolicy {
        AccessPolicy::new(&["com.apple.TextEdit".to_string(), "com.apple.Terminal".to_string()])
    }

    fn request(window_id: u32, screenshot: Shot, reference: Option<&str>) -> ObserveRequest {
        ObserveRequest {
            window_id,
            screenshot,
            tree: true,
            reference: reference.map(str::to_string),
            max_edge: 1000,
        }
    }

    #[test]
    fn only_allowed_windows_are_listed() {
        let (session, _) = session();
        let ids: Vec<u32> = session.list_windows(&policy()).unwrap().iter().map(|w| w.id).collect();
        assert_eq!(ids, [1]);
    }

    #[test]
    fn window_of_unlisted_app_is_refused_without_capturing() {
        let (session, fake) = session();
        let result = session.observe(&policy(), request(2, Shot::Window, None));
        assert_eq!(result.err(), Some(Error::NotAllowed("com.apple.Notes".to_string())));
        assert_eq!(
            session.observe(&policy(), request(3, Shot::Window, None)).err(),
            Some(Error::Forbidden("com.apple.Terminal".to_string()))
        );
        assert_eq!(
            session.observe(&policy(), request(4, Shot::Window, None)).err(),
            Some(Error::Unidentified("helper".to_string()))
        );
        assert_eq!(
            session.observe(&policy(), request(9, Shot::Window, None)).err(),
            Some(Error::WindowNotFound(9))
        );
        assert!(fake.captures.lock().unwrap().is_empty());
        assert_eq!(*fake.tree_reads.lock().unwrap(), 0);
    }

    #[test]
    fn stage_manager_thumbnail_is_refused_instead_of_misreported() {
        // Listed as 2000x1000, but accessibility says the real window is far larger.
        let (session, fake) = session_with(Fake { ax_window_size: Some((4000.0, 2000.0)), ..Fake::default() });
        let result = session.observe(&policy(), request(1, Shot::Window, None));
        assert_eq!(result.err(), Some(Error::Thumbnail));
        assert!(fake.captures.lock().unwrap().is_empty());

        let (session, _) = session_with(Fake { ax_window_size: Some((2000.0, 1000.0)), ..Fake::default() });
        assert!(session.observe(&policy(), request(1, Shot::Window, None)).is_ok());
    }

    #[test]
    fn empty_request_is_rejected() {
        let (session, _) = session();
        let mut nothing = request(1, Shot::None, None);
        nothing.tree = false;
        assert_eq!(session.observe(&policy(), nothing).err(), Some(Error::NothingRequested));
    }

    #[test]
    fn tree_coordinates_match_the_downscaled_screenshot() {
        let (session, fake) = session();
        let seen = session.observe(&policy(), request(1, Shot::Window, None)).unwrap();
        // 2000x1000 pt window at max_edge 1000 is halved; so is the button.
        assert!(seen.text.contains("1000x500 px"), "{}", seen.text);
        assert!(seen.text.contains("e2 button \"Save\" [100,50 50x20]"), "{}", seen.text);
        assert_eq!(seen.png, Some(vec![1, 2, 3]));
        let whole = Rect { x: 0.0, y: 0.0, width: 2000.0, height: 1000.0 };
        assert_eq!(*fake.captures.lock().unwrap(), [(1, whole, 1000, 500)]);
    }

    #[test]
    fn zoom_captures_the_element_and_needs_a_fresh_ref() {
        let (session, fake) = session();
        assert_eq!(
            session.observe(&policy(), request(1, Shot::Region, Some("e2"))).err(),
            Some(Error::StaleRef("e2".to_string())),
            "no observe yet"
        );

        session.observe(&policy(), request(1, Shot::None, None)).unwrap();
        session.observe(&policy(), request(1, Shot::Region, Some("e2"))).unwrap();
        let button = Rect { x: 200.0, y: 100.0, width: 100.0, height: 40.0 };
        assert_eq!(*fake.captures.lock().unwrap(), [(1, button, 200, 80)]);

        // A second observe issues new ids (e3, e4); the old ref must not resolve.
        session.observe(&policy(), request(1, Shot::None, None)).unwrap();
        assert_eq!(
            session.observe(&policy(), request(1, Shot::Region, Some("e2"))).err(),
            Some(Error::StaleRef("e2".to_string()))
        );
        assert!(session.observe(&policy(), request(1, Shot::Region, Some("e4"))).is_ok());
    }
}
