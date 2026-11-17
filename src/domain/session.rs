//! Use cases: what an agent can ask for, checked against the access policy.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::domain::policy::{Access, AccessPolicy};
use crate::domain::snapshot::{Filter, RefError, Snapshot, one_line};
use crate::domain::viewport::{Viewport, clamp_to_window, is_thumbnail};
use crate::ports::{
    ElementHandle, Input, Overlay, Pause, Rect, ScreenCapture, UiActions, UiTree, WindowInfo, WindowSource,
};

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
    /// Which elements of the tree are printed; all of them can still be referred to.
    pub filter: Filter,
}

pub struct WaitRequest {
    pub window_id: u32,
    /// What to wait for: it must name something (find or role).
    pub filter: Filter,
    /// Wait until nothing matches instead of until something does.
    pub gone: bool,
    /// Image size for coordinates when this window has not been observed yet.
    pub max_edge: u32,
}

pub struct Observation {
    pub text: String,
    pub png: Option<Vec<u8>>,
}

pub struct ListedWindow {
    pub window: WindowInfo,
    /// The window's app is the frontmost app.
    pub frontmost: bool,
    /// Only a Stage Manager thumbnail of the window is on screen.
    pub thumbnail: bool,
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
    Obscured(String),
    Thumbnail,
    OffScreen(u32),
    NeedsRealInput,
    Covered,
    DoubleClickCovered,
    NoView,
    PointOutside,
    BadKeys(String),
    Paused,
    NothingToWaitFor,
    /// What was waited for, whether it was to disappear, and for how long.
    WaitTimedOut { filter: String, gone: bool, waited_ms: u128 },
    WaitCancelled,
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
            Self::RefNotVisible(r) => write!(
                f,
                "{r} has no visible area inside the window; scroll it into view and observe again"
            ),
            Self::Thumbnail => write!(
                f,
                "this window is only a Stage Manager thumbnail because its app is in the \
                 background, so it cannot be observed. Call activate with this window_id to \
                 bring it to the front, then observe again"
            ),
            Self::OffScreen(id) => write!(
                f,
                "window {id} is not on screen: it is minimised, its app is hidden, or it is on \
                 another Space. Call activate with this window_id to bring it to the front, then \
                 try again"
            ),
            Self::NeedsRealInput => write!(
                f,
                "this cannot be done through accessibility alone. Retry with mode \"auto\" to \
                 allow real pointer and keyboard input; the app is then brought to the front"
            ),
            Self::Obscured(r) => write!(
                f,
                "{r} is not what is shown at its position right now: it is scrolled out of view \
                 or something in the window covers it. Scroll it into view and observe again, \
                 or use x,y from a fresh screenshot"
            ),
            Self::Covered => write!(
                f,
                "another window, a menu or a system overlay covers that point, so real input \
                 would hit the wrong target. If it is a menu or popover of this app, press_key \
                 escape closes it; otherwise ask the user to uncover the window"
            ),
            Self::DoubleClickCovered => write!(
                f,
                "the double click was stopped because something came to cover that point. Its \
                 first click may already have landed: observe before trying again"
            ),
            Self::NoView => write!(
                f,
                "x,y are pixels of the window screenshot, so they need a prior observe of this \
                 window; call observe first, or pass `ref`"
            ),
            Self::PointOutside => write!(f, "x,y are outside the window image"),
            Self::BadKeys(message) => write!(f, "{message}"),
            Self::Paused => write!(
                f,
                "the user has paused agent input with the emergency stop (ctrl+option+cmd+escape). \
                 Do not retry: tell the user, and continue only after they resume it"
            ),
            Self::NothingToWaitFor => write!(f, "wait_for needs `find` or `role` to know what to wait for"),
            Self::WaitTimedOut { filter, gone: true, waited_ms } => write!(
                f,
                "after {waited_ms} ms an element matching {filter} is still there. Refs from \
                 earlier observes no longer work: call observe to see the current state"
            ),
            Self::WaitTimedOut { filter, gone: false, waited_ms } => write!(
                f,
                "after {waited_ms} ms there is still no element matching {filter}. Refs from \
                 earlier observes no longer work: call observe to see the current state"
            ),
            Self::WaitCancelled => write!(f, "the wait was cancelled. Call observe to see the current state"),
            Self::System(message) => write!(f, "{message}"),
        }
    }
}

/// Everything the session needs from the operating system.
pub struct Ports {
    pub windows: Arc<dyn WindowSource>,
    pub capture: Arc<dyn ScreenCapture>,
    pub tree: Arc<dyn UiTree>,
    pub actions: Arc<dyn UiActions>,
    pub input: Arc<dyn Input>,
    pub overlay: Arc<dyn Overlay>,
    pub pause: Arc<dyn Pause>,
}

pub struct Session {
    pub(super) ports: Ports,
    pub(super) state: Mutex<State>,
}

pub(super) struct State {
    snapshot: Option<Snapshot>,
    next_element_id: u64,
    /// The window and scale of the latest whole-window observe: the pixel
    /// space in which the agent gives x,y.
    pub(super) view: Option<(u32, Viewport)>,
}

impl Session {
    /// `first_element_id` should differ between server runs, so that a ref an
    /// agent kept from a previous run is unlikely to name a new element.
    pub fn new(ports: Ports, first_element_id: u64) -> Self {
        let state = State { snapshot: None, next_element_id: first_element_id, view: None };
        Self { ports, state: Mutex::new(state) }
    }

    /// Whether the user's emergency stop is on.
    pub fn input_paused(&self) -> bool {
        self.ports.pause.is_paused()
    }

    /// Windows of allowed apps only.
    pub fn list_windows(&self, policy: &AccessPolicy) -> Result<Vec<ListedWindow>, Error> {
        let windows = self.ports.windows.windows().map_err(Error::System)?;
        let front = self.ports.windows.frontmost_pid();
        // Apps that did not answer: each request waits for a timeout, so one
        // hung app is asked once, not once per window.
        let mut unanswered = Vec::new();
        let listed = windows
            .into_iter()
            // Filtered first: reading a window's real frame talks to its app,
            // and apps that are not allowed are never talked to.
            .filter(|w| access(policy, w) == Access::Allowed)
            .map(|window| {
                // Unknown counts as not a thumbnail: listing must not fail
                // because one app does not answer accessibility requests.
                let mut thumbnail = false;
                if window.on_screen && !unanswered.contains(&window.pid) {
                    match self.ports.tree.window_frame(window.pid, window.id) {
                        Ok(actual) => thumbnail = actual.is_some_and(|actual| is_thumbnail(window.frame, actual)),
                        Err(_) => unanswered.push(window.pid),
                    }
                }
                ListedWindow { frontmost: front == Some(window.pid), window, thumbnail }
            })
            .collect();
        Ok(listed)
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
        let mut text = header(&window, &viewport);
        if request.tree
            && let Some(tree) = self.read_tree(&window, &viewport, &request.filter, |_| true)?
        {
            text.push_str(&tree);
        }

        let png = match request.screenshot {
            Shot::Window => Some(
                self.ports
                    .capture
                    .capture(window.id, window.pid, whole, width, height)
                    .map_err(Error::System)?,
            ),
            _ => None,
        };
        // Only a successful observe defines the pixel space for x,y.
        self.state.lock().unwrap().view = Some((window.id, viewport));
        Ok(Observation { text, png })
    }

    /// Polls the window's tree every `poll` until something matches the
    /// filter (or, with `gone`, nothing does), and returns what a tree-only
    /// observe with that filter would. Gives up after `timeout`, and stops
    /// before the next poll once `cancelled` says so.
    pub fn wait_for(
        &self,
        policy: &AccessPolicy,
        request: WaitRequest,
        timeout: Duration,
        poll: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<String, Error> {
        if !request.filter.names_something() {
            return Err(Error::NothingToWaitFor);
        }
        let deadline = Instant::now() + timeout;
        loop {
            if cancelled() {
                return Err(Error::WaitCancelled);
            }
            // Checked on every poll: the window can close or go off screen meanwhile.
            let window = self.allowed_window(policy, request.window_id)?;
            // The scale of the agent's last screenshot of this window, so its
            // x,y keep pointing at the same places, over the window as it is now.
            let whole = Rect { x: 0.0, y: 0.0, width: window.frame.width, height: window.frame.height };
            let view = self.state.lock().unwrap().view;
            let viewport = match view {
                Some((id, seen)) if id == window.id => Viewport { region: whole, scale: seen.scale },
                _ => Viewport::fit(whole, 1.0, request.max_edge),
            };
            let done = |matches: usize| (matches > 0) != request.gone;
            if let Some(tree) = self.read_tree(&window, &viewport, &request.filter, done)? {
                self.state.lock().unwrap().view = Some((window.id, viewport));
                return Ok(header(&window, &viewport) + &tree);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::WaitTimedOut {
                    filter: request.filter.to_string(),
                    gone: request.gone,
                    waited_ms: timeout.as_millis(),
                });
            }
            thread::sleep(poll.min(left));
        }
    }

    /// Reads the window's element tree. When `accept` approves the number of
    /// elements `filter` matches, the tree becomes the snapshot refs resolve
    /// against and is returned rendered through `filter`; otherwise no refs
    /// work until the next successful read.
    fn read_tree(
        &self,
        window: &WindowInfo,
        viewport: &Viewport,
        filter: &Filter,
        accept: impl FnOnce(usize) -> bool,
    ) -> Result<Option<String>, Error> {
        let tree = self
            .ports
            .tree
            .tree(window.pid, window.id, MAX_TREE_NODES)
            .map_err(Error::System)?;
        // Numbered and stored under one lock, so two reads never issue the same ids.
        let mut state = self.state.lock().unwrap();
        // Reading a tree retires the handles of the previous one, so its refs
        // must fail here rather than after an action has begun.
        state.snapshot = None;
        if tree.root.frame.is_some_and(|actual| is_thumbnail(window.frame, actual)) {
            return Err(Error::Thumbnail);
        }
        let origin = (window.frame.x, window.frame.y);
        let snapshot = Snapshot::build(window.id, window.pid, origin, tree.root, state.next_element_id);
        if !accept(snapshot.elements.iter().filter(|e| filter.matches(e)).count()) {
            return Ok(None);
        }
        state.next_element_id += snapshot.elements.len() as u64;
        let mut text = snapshot.render(viewport, filter);
        if tree.truncated {
            text.push_str("(tree incomplete: some elements were skipped because of the size or time limit, or could not be read)\n");
        }
        state.snapshot = Some(snapshot);
        Ok(Some(text))
    }

    /// A magnified screenshot of one element. Leaves the snapshot untouched.
    fn zoom(&self, window: &WindowInfo, request: &ObserveRequest) -> Result<Observation, Error> {
        let reference = request.reference.as_deref().ok_or(Error::RefRequired)?;
        // Where the element is now, not where it was when observed.
        let frame = self.live_frame(window, reference)?;
        let relative = Rect { x: frame.x - window.frame.x, y: frame.y - window.frame.y, ..frame };
        let region = clamp_to_window(relative, window.frame.width, window.frame.height)
            .ok_or_else(|| Error::RefNotVisible(reference.to_string()))?;
        let viewport = Viewport::fit(region, ZOOM_MAX_SCALE, request.max_edge);
        let (width, height) = viewport.image_size();
        let png = self
            .ports
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

    /// The live element behind a ref of the latest observe of this window.
    pub(super) fn element(&self, window: &WindowInfo, reference: &str) -> Result<ElementHandle, Error> {
        let state = self.state.lock().unwrap();
        let stale = || Error::StaleRef(reference.to_string());
        let snapshot = state.snapshot.as_ref().ok_or_else(stale)?;
        match snapshot.resolve(window.id, window.pid, reference) {
            Ok(element) => Ok(element.handle),
            Err(RefError::Malformed) => Err(Error::BadRef(reference.to_string())),
            Err(RefError::Stale) => Err(stale()),
        }
    }

    /// The element's current frame in screen points.
    pub(super) fn live_frame(&self, window: &WindowInfo, reference: &str) -> Result<Rect, Error> {
        let handle = self.element(window, reference)?;
        self.ports
            .actions
            .frame(handle)
            .map_err(Error::System)?
            .ok_or_else(|| Error::RefNotVisible(reference.to_string()))
    }

    /// A window the policy allows that is on screen, so it can be seen and acted on.
    pub(super) fn allowed_window(&self, policy: &AccessPolicy, window_id: u32) -> Result<WindowInfo, Error> {
        let window = self.permitted_window(policy, window_id)?;
        match window.on_screen {
            true => Ok(window),
            false => Err(Error::OffScreen(window_id)),
        }
    }

    /// A window the policy allows, wherever it is.
    pub(super) fn permitted_window(&self, policy: &AccessPolicy, window_id: u32) -> Result<WindowInfo, Error> {
        let window = self
            .ports
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

/// `window 1 "title" (app), 1280x640 px`: the first line of a tree or screenshot.
fn header(window: &WindowInfo, viewport: &Viewport) -> String {
    let (width, height) = viewport.image_size();
    format!(
        "window {} \"{}\" ({}), {width}x{height} px\n",
        window.id,
        one_line(&window.title),
        one_line(&window.app_name)
    )
}

/// An app without a bundle id cannot be allowlisted, so it is never allowed.
fn access(policy: &AccessPolicy, window: &WindowInfo) -> Access {
    window.bundle_id.as_deref().map_or(Access::NotAllowed, |id| policy.check(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::actions::At;
    use crate::domain::testing::{Fake, policy, session, session_with};

    fn request(window_id: u32, screenshot: Shot, reference: Option<&str>) -> ObserveRequest {
        ObserveRequest {
            window_id,
            screenshot,
            tree: true,
            reference: reference.map(str::to_string),
            max_edge: 1000,
            filter: Filter::default(),
        }
    }

    fn wait(window_id: u32, find: &str, gone: bool) -> WaitRequest {
        let filter = Filter::new(Some(find.to_string()), None, false, None);
        WaitRequest { window_id, filter, gone, max_edge: 1000 }
    }

    const POLL: Duration = Duration::from_millis(1);
    const LONG: Duration = Duration::from_secs(5);

    #[test]
    fn only_allowed_windows_are_listed() {
        let (session, _) = session();
        let ids: Vec<u32> = session.list_windows(&policy()).unwrap().iter().map(|w| w.window.id).collect();
        assert_eq!(ids, [1]);
    }

    #[test]
    fn off_screen_windows_are_listed_only_for_allowed_apps() {
        let (session, fake) = session_with(Fake { off_screen: vec![1, 2], ..Fake::default() });
        let listed = session.list_windows(&policy()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].window.id, 1);
        assert!(!listed[0].window.on_screen);
        assert!(!listed[0].thumbnail);
        assert!(fake.frame_reads.lock().unwrap().is_empty(), "an off-screen window has no thumbnail");
    }

    #[test]
    fn list_marks_thumbnails_asking_only_about_allowed_windows() {
        let (session, fake) = session_with(Fake { ax_window_size: Some((4000.0, 2000.0)), ..Fake::default() });
        let listed = session.list_windows(&policy()).unwrap();
        assert!(listed[0].thumbnail);
        assert_eq!(*fake.frame_reads.lock().unwrap(), [1]);

        let (session, _) = session_with(Fake { ax_window_size: Some((2000.0, 1000.0)), ..Fake::default() });
        assert!(!session.list_windows(&policy()).unwrap()[0].thumbnail);

        // An app that does not answer is listed without the flag, and asked
        // only once however many windows it has.
        let (session, fake) =
            session_with(Fake { frame_read_fails: true, second_window: true, ..Fake::default() });
        let listed = session.list_windows(&policy()).unwrap();
        assert_eq!(listed.iter().map(|l| l.window.id).collect::<Vec<_>>(), [1, 5]);
        assert!(listed.iter().all(|l| !l.thumbnail));
        assert_eq!(*fake.frame_reads.lock().unwrap(), [1]);
    }

    #[test]
    fn list_marks_windows_of_the_frontmost_app() {
        let (session, _) = session_with(Fake { frontmost: Some(1), second_window: true, ..Fake::default() });
        let listed = session.list_windows(&policy()).unwrap();
        assert!(listed.iter().all(|l| l.frontmost), "both TextEdit windows");

        let (session, _) = session_with(Fake { frontmost: Some(2), ..Fake::default() });
        assert!(!session.list_windows(&policy()).unwrap()[0].frontmost);
    }

    #[test]
    fn off_screen_window_is_refused_without_capturing_or_reading() {
        let (session, fake) = session_with(Fake { off_screen: vec![1], ..Fake::default() });
        assert_eq!(session.observe(&policy(), request(1, Shot::Window, None)).err(), Some(Error::OffScreen(1)));
        assert_eq!(session.observe(&policy(), request(1, Shot::Region, Some("e2"))).err(), Some(Error::OffScreen(1)));
        assert!(fake.captures.lock().unwrap().is_empty());
        assert_eq!(*fake.tree_reads.lock().unwrap(), 0);
        assert!(fake.log().is_empty());
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

    #[test]
    fn filter_hides_lines_but_not_refs_or_the_screenshot() {
        let (session, fake) = session();
        let mut windows_only = request(1, Shot::Window, None);
        windows_only.filter = Filter::new(None, Some("window".to_string()), false, None);
        let seen = session.observe(&policy(), windows_only).unwrap();
        assert_eq!(
            seen.text,
            "window 1 \"t\" (com.apple.TextEdit), 1000x500 px\n\
             e1 window\n\
             (1 of 2 elements shown; the filter hides the rest)\n"
        );
        assert!(seen.png.is_some());
        // The button, e2, was not printed but is part of the snapshot.
        session.observe(&policy(), request(1, Shot::Region, Some("e2"))).unwrap();
        assert_eq!(fake.captures.lock().unwrap().len(), 2);
    }

    #[test]
    fn wait_for_returns_the_element_once_it_appears() {
        let (session, fake) = session_with(Fake { renamed_after_reads: Some(2), ..Fake::default() });
        let found = session.wait_for(&policy(), wait(1, "DONE", false), LONG, POLL, &|| false).unwrap();
        assert_eq!(
            found,
            "window 1 \"t\" (com.apple.TextEdit), 1000x500 px\n\
             e2 button \"Done\" [100,50 50x20]\n\
             (1 of 2 elements shown; the filter hides the rest)\n"
        );
        assert_eq!(*fake.tree_reads.lock().unwrap(), 3);
        // Polls that matched nothing issued no ids, and the returned ref works at once.
        session.observe(&policy(), request(1, Shot::Region, Some("e2"))).unwrap();
    }

    #[test]
    fn wait_for_gone_returns_once_nothing_matches() {
        let (session, fake) = session_with(Fake { renamed_after_reads: Some(1), ..Fake::default() });
        let gone = session.wait_for(&policy(), wait(1, "save", true), LONG, POLL, &|| false).unwrap();
        assert_eq!(
            gone,
            "window 1 \"t\" (com.apple.TextEdit), 1000x500 px\n\
             (no element matches the filter; the tree has 2)\n"
        );
        assert_eq!(*fake.tree_reads.lock().unwrap(), 2);
    }

    #[test]
    fn wait_for_times_out_and_earlier_refs_fail_before_any_action() {
        let (session, fake) = session();
        session.observe(&policy(), request(1, Shot::None, None)).unwrap();
        let result = session.wait_for(&policy(), wait(1, "missing", false), Duration::from_millis(20), POLL, &|| false);
        assert_eq!(
            result.err(),
            Some(Error::WaitTimedOut { filter: "find \"missing\"".to_string(), gone: false, waited_ms: 20 })
        );
        // The polls retired the button's handle, so the ref must fail before
        // the app is brought forward for a scroll that could not work.
        let stale = Some(Error::StaleRef("e2".to_string()));
        assert_eq!(session.observe(&policy(), request(1, Shot::Region, Some("e2"))).err(), stale);
        assert_eq!(session.scroll(&policy(), 1, Some(&At::Element("e2".to_string())), 0, 3).err(), stale);
        assert!(fake.log().is_empty());

        let still_there = session.wait_for(&policy(), wait(1, "save", true), Duration::ZERO, POLL, &|| false);
        assert!(matches!(still_there, Err(Error::WaitTimedOut { gone: true, .. })));
    }

    #[test]
    fn wait_for_matches_a_role() {
        let (session, _) = session();
        let buttons = WaitRequest { filter: Filter::new(None, Some("BUTTON".to_string()), false, None), ..wait(1, "", false) };
        let found = session.wait_for(&policy(), buttons, LONG, POLL, &|| false).unwrap();
        assert!(found.contains("\ne2 button \"Save\" [100,50 50x20]\n"), "{found}");
        session.observe(&policy(), request(1, Shot::Region, Some("e2"))).unwrap();
    }

    #[test]
    fn a_thumbnail_ends_the_wait_and_drops_the_refs() {
        let (session, fake) = session_with(Fake { thumbnail_after_reads: Some(1), ..Fake::default() });
        session.observe(&policy(), request(1, Shot::None, None)).unwrap();
        let result = session.wait_for(&policy(), wait(1, "missing", false), LONG, POLL, &|| false);
        assert_eq!(result.err(), Some(Error::Thumbnail));
        assert_eq!(*fake.tree_reads.lock().unwrap(), 2);
        assert_eq!(
            session.observe(&policy(), request(1, Shot::Region, Some("e2"))).err(),
            Some(Error::StaleRef("e2".to_string()))
        );

        // The same for observe: its tree read has retired the previous handles.
        let (session, _) = session_with(Fake { thumbnail_after_reads: Some(1), ..Fake::default() });
        session.observe(&policy(), request(1, Shot::None, None)).unwrap();
        assert_eq!(session.observe(&policy(), request(1, Shot::None, None)).err(), Some(Error::Thumbnail));
        assert_eq!(
            session.observe(&policy(), request(1, Shot::Region, Some("e2"))).err(),
            Some(Error::StaleRef("e2".to_string()))
        );
    }

    #[test]
    fn a_cancelled_wait_stops_before_the_next_poll() {
        let (session, fake) = session();
        let checks = std::cell::Cell::new(0);
        let cancelled = || {
            checks.set(checks.get() + 1);
            checks.get() > 1
        };
        let result = session.wait_for(&policy(), wait(1, "missing", false), LONG, POLL, &cancelled);
        assert_eq!(result.err(), Some(Error::WaitCancelled));
        assert_eq!(*fake.tree_reads.lock().unwrap(), 1);
    }

    #[test]
    fn wait_for_checks_the_window_and_the_request_before_reading() {
        let (session, fake) = session_with(Fake { off_screen: vec![1], ..Fake::default() });
        assert_eq!(session.wait_for(&policy(), wait(1, "save", false), LONG, POLL, &|| false).err(), Some(Error::OffScreen(1)));
        assert_eq!(
            session.wait_for(&policy(), wait(2, "save", false), LONG, POLL, &|| false).err(),
            Some(Error::NotAllowed("com.apple.Notes".to_string()))
        );
        let nothing = WaitRequest { filter: Filter::new(Some(String::new()), None, true, Some(3)), ..wait(1, "", false) };
        assert_eq!(session.wait_for(&policy(), nothing, LONG, POLL, &|| false).err(), Some(Error::NothingToWaitFor));
        assert_eq!(*fake.tree_reads.lock().unwrap(), 0);
    }

    #[test]
    fn wait_for_keeps_the_scale_of_the_previous_observe() {
        let (session, _) = session();
        let mut small = request(1, Shot::None, None);
        small.max_edge = 500;
        session.observe(&policy(), small).unwrap();
        let found = session.wait_for(&policy(), wait(1, "save", false), LONG, POLL, &|| false).unwrap();
        assert!(found.starts_with("window 1 \"t\" (com.apple.TextEdit), 500x250 px\n"), "{found}");
        assert!(found.contains("button \"Save\" [50,25 25x10]"), "{found}");
    }
}
