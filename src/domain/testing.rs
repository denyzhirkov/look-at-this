//! A scripted operating system for use-case tests.

use std::sync::{Arc, Mutex};

use crate::domain::policy::AccessPolicy;
use crate::domain::session::{Ports, Session};
use crate::ports::{
    Button, ElementHandle, Input, KeyCombo, Overlay, Point, PortResult, Rect, ScreenCapture, UiAction,
    UiActions, UiNode, UiTree, UiTreeSnapshot, WindowInfo, WindowSource,
};

/// Handle of the one button in the fake tree.
pub const BUTTON: ElementHandle = 7;

/// Four windows: 1 TextEdit (allowed), 2 Notes (not allowed), 3 Terminal
/// (forbidden), 4 an app without a bundle id. Window 1 is 2000x1000 at
/// (100,50) and holds a "Save" button at screen (300,150), 100x40.
#[derive(Default)]
pub struct Fake {
    pub captures: Mutex<Vec<(u32, Rect, u32, u32)>>,
    pub tree_reads: Mutex<u32>,
    /// Size accessibility reports for the window; `None` reports no frame.
    pub ax_window_size: Option<(f64, f64)>,
    /// Where the agent's cursor was shown, in order.
    pub pointed: Mutex<Vec<Point>>,
    /// Every action and real-input call, in order.
    pub log: Mutex<Vec<String>>,
    /// Whether accessibility actions and text insertion succeed.
    pub accessible: bool,
    /// Where the button is now, if it moved since it was observed.
    pub button_moved_to: Option<Rect>,
    /// Whether something else is on top of window 1, so real input is refused.
    pub covered: bool,
    /// Whether the accessibility hit test finds something else at the button.
    pub obscured: bool,
}

impl Fake {
    fn record(&self, entry: String) {
        self.log.lock().unwrap().push(entry);
    }

    pub fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

const BUTTON_FRAME: Rect = Rect { x: 300.0, y: 150.0, width: 100.0, height: 40.0 };

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
            handle: BUTTON,
            role: "AXButton".to_string(),
            title: "Save".to_string(),
            frame: Some(BUTTON_FRAME),
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

impl UiActions for Fake {
    fn is_at(&self, element: ElementHandle, _point: Point) -> PortResult<bool> {
        Ok(element == BUTTON && !self.obscured)
    }

    fn frame(&self, element: ElementHandle) -> PortResult<Option<Rect>> {
        Ok((element == BUTTON).then_some(self.button_moved_to.unwrap_or(BUTTON_FRAME)))
    }

    fn perform(&self, element: ElementHandle, action: UiAction) -> PortResult<bool> {
        if self.accessible {
            self.record(format!("ax {action:?} {element}"));
        }
        Ok(self.accessible)
    }

    fn insert_text(&self, element: ElementHandle, text: &str, replace: bool) -> PortResult<bool> {
        if self.accessible {
            self.record(format!("ax insert {element} {text:?} replace={replace}"));
        }
        Ok(self.accessible)
    }
}

impl Input for Fake {
    fn activate(&self, pid: i32, window_id: u32) -> PortResult<()> {
        self.record(format!("activate pid {pid} window {window_id}"));
        Ok(())
    }

    fn click(&self, window_id: u32, point: Point, button: Button, count: u8) -> PortResult<bool> {
        assert_eq!(window_id, 1, "real input must name the access-checked window");
        if !self.covered {
            self.record(format!("click {},{} {button:?} x{count}", point.x, point.y));
        }
        Ok(!self.covered)
    }

    fn scroll(&self, window_id: u32, point: Point, dx: i32, dy: i32) -> PortResult<bool> {
        assert_eq!(window_id, 1, "real input must name the access-checked window");
        if !self.covered {
            self.record(format!("scroll {},{} by {dx},{dy}", point.x, point.y));
        }
        Ok(!self.covered)
    }

    fn type_text(&self, pid: i32, text: &str) -> PortResult<()> {
        self.record(format!("type pid {pid} {text:?}"));
        Ok(())
    }

    fn press_key(&self, pid: i32, combo: KeyCombo) -> PortResult<()> {
        let m = combo.modifiers;
        let mods: String = [(m.cmd, "cmd+"), (m.ctrl, "ctrl+"), (m.alt, "alt+"), (m.shift, "shift+")]
            .iter()
            .filter_map(|(on, name)| on.then_some(*name))
            .collect();
        self.record(format!("key pid {pid} {mods}{:?}", combo.key));
        Ok(())
    }
}

impl Overlay for Fake {
    fn point_at(&self, point: Point) {
        self.pointed.lock().unwrap().push(point);
    }
}

pub fn session() -> (Session, Arc<Fake>) {
    session_with(Fake::default())
}

pub fn session_with(fake: Fake) -> (Session, Arc<Fake>) {
    let fake = Arc::new(fake);
    let ports = Ports {
        windows: fake.clone(),
        capture: fake.clone(),
        tree: fake.clone(),
        actions: fake.clone(),
        input: fake.clone(),
        overlay: fake.clone(),
    };
    (Session::new(ports, 1), fake)
}

/// Terminal is allowlisted on purpose: the hard denylist must still win.
pub fn policy() -> AccessPolicy {
    AccessPolicy::new(&["com.apple.TextEdit".to_string(), "com.apple.Terminal".to_string()])
}
