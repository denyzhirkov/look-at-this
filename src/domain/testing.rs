//! A scripted operating system for use-case tests.

use std::sync::{Arc, Mutex};

use crate::domain::policy::AccessPolicy;
use crate::domain::session::{Ports, Session};
use crate::ports::{
    Button, ElementHandle, Input, KeyCombo, Overlay, Pause, Point, PortResult, Rect, ScreenCapture, UiAction,
    UiActions, UiNode, UiTree, UiTreeSnapshot, WindowInfo, WindowSource,
};

/// Four windows: 1 TextEdit (allowed), 2 Notes (not allowed), 3 Terminal
/// (forbidden), 4 an app without a bundle id, all on screen unless listed in
/// `off_screen`; each window's pid is its id. Window 1 is 2000x1000 at
/// (100,50) and holds a "Save" button at screen (300,150), 100x40.
/// As with the real adapter, each tree read retires the handles of the one
/// before: read N's window is handle N01 and its button N07.
#[derive(Default)]
pub struct Fake {
    pub captures: Mutex<Vec<(u32, Rect, u32, u32)>>,
    pub tree_reads: Mutex<u32>,
    /// Size accessibility reports for the window; `None` reports no frame.
    pub ax_window_size: Option<(f64, f64)>,
    /// Windows whose accessibility frame was asked for alone, in order.
    pub frame_reads: Mutex<Vec<u32>>,
    /// Whether asking for a window's accessibility frame fails.
    pub frame_read_fails: bool,
    /// Windows that are minimised, hidden or on another Space.
    pub off_screen: Vec<u32>,
    /// Whether TextEdit also has window 5: same process (pid 1) and frame as
    /// window 1. For listing only.
    pub second_window: bool,
    /// Process id of the frontmost app.
    pub frontmost: Option<i32>,
    /// The emergency stop.
    pub paused: Mutex<bool>,
    /// Whether the user hits the emergency stop while the app comes forward.
    pub pause_once_activated: bool,
    /// Whether the user hits the emergency stop as soon as typing starts.
    pub pause_once_typing: bool,
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
    /// After this many tree reads the button is titled "Done" instead of "Save".
    pub renamed_after_reads: Option<u32>,
    /// After this many tree reads only a Stage Manager thumbnail is shown.
    pub thumbnail_after_reads: Option<u32>,
}

impl Fake {
    fn record(&self, entry: String) {
        self.log.lock().unwrap().push(entry);
    }

    pub fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// Window 1 as accessibility reports it.
    fn ax_frame(&self) -> Option<Rect> {
        self.ax_window_size.map(|(width, height)| Rect { x: 100.0, y: 50.0, width, height })
    }

    /// Whether `element` is the button of the latest tree; an element of an
    /// older tree fails as it does in the real adapter.
    fn is_button(&self, element: ElementHandle) -> PortResult<bool> {
        if element / 100 != u64::from(*self.tree_reads.lock().unwrap()) {
            return Err("the element no longer exists".to_string());
        }
        Ok(element % 100 == 7)
    }
}

/// Handle of the button in tree read `read`.
fn button_handle(read: u32) -> ElementHandle {
    u64::from(read) * 100 + 7
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
            on_screen: !self.off_screen.contains(&id),
        };
        let mut anonymous = window(4, "", 0.0);
        anonymous.bundle_id = None;
        anonymous.app_name = "helper".to_string();
        let mut windows = vec![
            window(1, "com.apple.TextEdit", 100.0),
            window(2, "com.apple.Notes", 0.0),
            window(3, "com.apple.Terminal", 0.0),
            anonymous,
        ];
        if self.second_window {
            windows.push(WindowInfo { pid: 1, ..window(5, "com.apple.TextEdit", 100.0) });
        }
        Ok(windows)
    }

    fn frontmost_pid(&self) -> Option<i32> {
        self.frontmost
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
        let reads = {
            let mut reads = self.tree_reads.lock().unwrap();
            *reads += 1;
            *reads
        };
        let renamed = self.renamed_after_reads.is_some_and(|n| reads > n);
        let thumbnail = self.thumbnail_after_reads.is_some_and(|n| reads > n);
        let button = UiNode {
            handle: button_handle(reads),
            role: "AXButton".to_string(),
            title: if renamed { "Done" } else { "Save" }.to_string(),
            frame: Some(BUTTON_FRAME),
            enabled: true,
            ..UiNode::default()
        };
        let root = UiNode {
            handle: u64::from(reads) * 100 + 1,
            role: "AXWindow".to_string(),
            frame: match thumbnail {
                true => Some(Rect { x: 100.0, y: 50.0, width: 4000.0, height: 2000.0 }),
                false => self.ax_frame(),
            },
            enabled: true,
            children: vec![button],
            ..UiNode::default()
        };
        Ok(UiTreeSnapshot { root, truncated: false })
    }

    fn window_frame(&self, _pid: i32, window_id: u32) -> PortResult<Option<Rect>> {
        self.frame_reads.lock().unwrap().push(window_id);
        match self.frame_read_fails {
            true => Err("the app is not answering".to_string()),
            false => Ok(self.ax_frame()),
        }
    }
}

impl UiActions for Fake {
    fn is_at(&self, element: ElementHandle, _point: Point) -> PortResult<bool> {
        Ok(self.is_button(element)? && !self.obscured)
    }

    fn frame(&self, element: ElementHandle) -> PortResult<Option<Rect>> {
        Ok(self.is_button(element)?.then_some(self.button_moved_to.unwrap_or(BUTTON_FRAME)))
    }

    fn perform(&self, element: ElementHandle, action: UiAction) -> PortResult<bool> {
        self.is_button(element)?;
        if self.accessible {
            self.record(format!("ax {action:?} {element}"));
        }
        Ok(self.accessible)
    }

    fn insert_text(&self, element: ElementHandle, text: &str, replace: bool) -> PortResult<bool> {
        self.is_button(element)?;
        if self.accessible {
            self.record(format!("ax insert {element} {text:?} replace={replace}"));
        }
        Ok(self.accessible)
    }
}

impl Input for Fake {
    fn activate(&self, pid: i32, window_id: u32) -> PortResult<()> {
        self.record(format!("activate pid {pid} window {window_id}"));
        if self.pause_once_activated {
            *self.paused.lock().unwrap() = true;
        }
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
        if self.pause_once_typing {
            *self.paused.lock().unwrap() = true;
        }
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

impl Pause for Fake {
    fn is_paused(&self) -> bool {
        *self.paused.lock().unwrap()
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
        pause: fake.clone(),
    };
    (Session::new(ports, 1), fake)
}

/// Terminal is allowlisted on purpose: the hard denylist must still win.
pub fn policy() -> AccessPolicy {
    AccessPolicy::new(&["com.apple.TextEdit".to_string(), "com.apple.Terminal".to_string()])
}
