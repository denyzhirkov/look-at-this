//! A scripted operating system for use-case tests.

use std::sync::{Arc, Mutex};

use crate::domain::policy::AccessPolicy;
use crate::domain::session::{Ports, Session};
use crate::domain::session::{ObserveRequest, Shot};
use crate::ports::{
    Button, ControlState, DragCancel, DragOutcome, DragUndo, ElementHandle, Input, KeyCombo, LiveValue, MenuChoice, MenuItem, MenuKind, MenuOption, MenuStep,
    Overlay, Pause, Point, PortResult, Rect, ScreenCapture, UiAction, UiActions, UiNode, UiTree, UiTreeSnapshot,
    WindowInfo, WindowSource,
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
    /// What accessibility reports for the button read live; `None` reports
    /// a plain button. An accessible press flips a toggle (a radio button or
    /// a mixed checkbox turns on), and set_value updates it.
    pub control: Mutex<Option<ControlState>>,
    /// Step that a number set on the control snaps to.
    pub step: Option<f64>,
    /// Items of the menu the control opens as a pop-up button.
    pub options: Vec<MenuOption>,
    /// Whether the user hits the emergency stop while that menu opens.
    pub pause_once_menu_open: bool,
    /// Whether the control can be read only once: every later read fails.
    pub control_read_once: bool,
    pub control_reads: Mutex<u32>,
    /// The menu bar of TextEdit (pid 1).
    pub menu_bar: Vec<FakeMenu>,
    /// Whether window 1 is not TextEdit's main window.
    pub not_main: bool,
    /// Whether the user hits the emergency stop while the menus are read.
    pub pause_once_menu_read: bool,
    /// The title the item to press has by the time it is pressed.
    pub renamed_at_press: Option<String>,
    /// The kind the item to press has by the time it is pressed.
    pub kind_at_press: Option<MenuKind>,
    /// The size window 1 has once it has been brought forward.
    pub resized_on_activate: Option<(f64, f64)>,
    /// A slider at this screen frame after the button: handle N08, and `e3`
    /// in `observed`.
    pub slider: Option<Rect>,
    /// How a drag that was started ends; `None` drops it.
    pub drag_outcome: Option<DragOutcome>,
    /// Where a drag cut short by the emergency stop is released; `None`: at its start.
    pub drag_undo: Option<DragUndo>,
    /// Whether the user hits the emergency stop once a drag has started.
    pub pause_once_dragging: bool,
}

/// A menu item and, if it has a submenu, that submenu's items.
pub struct FakeMenu {
    pub item: MenuItem,
    pub items: Vec<FakeMenu>,
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

    /// Whether `element` is the slider of the latest tree, if there is one.
    fn is_slider(&self, element: ElementHandle) -> bool {
        self.slider.is_some() && element == u64::from(*self.tree_reads.lock().unwrap()) * 100 + 8
    }

    /// The items `path` leads to, checked the way the real adapter checks
    /// them; `None` if an item is not at its index with its title, or is not
    /// of kind `App`.
    fn menu_level(&self, path: &[MenuStep]) -> Option<&[FakeMenu]> {
        let mut level = self.menu_bar.as_slice();
        for step in path {
            let fits = |menu: &&FakeMenu| menu.item.title == step.title && menu.item.kind == MenuKind::App;
            level = &level.get(step.index).filter(fits)?.items;
        }
        Some(level)
    }
}

/// `Format > Font`, or `menu bar` for the empty path.
fn menu_path(path: &[MenuStep]) -> String {
    match path.is_empty() {
        true => "menu bar".to_string(),
        false => path.iter().map(|step| step.title.as_str()).collect::<Vec<_>>().join(" > "),
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
        let mut textedit = window(1, "com.apple.TextEdit", 100.0);
        if let Some((width, height)) = self.resized_on_activate
            && self.log().iter().any(|entry| entry.starts_with("activate"))
        {
            textedit.frame = Rect { width, height, ..textedit.frame };
        }
        let mut windows = vec![
            textedit,
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
        let slider = self.slider.map(|frame| UiNode {
            handle: u64::from(reads) * 100 + 8,
            role: "AXSlider".to_string(),
            frame: Some(frame),
            enabled: true,
            ..UiNode::default()
        });
        let root = UiNode {
            handle: u64::from(reads) * 100 + 1,
            role: "AXWindow".to_string(),
            frame: match thumbnail {
                true => Some(Rect { x: 100.0, y: 50.0, width: 4000.0, height: 2000.0 }),
                false => self.ax_frame(),
            },
            enabled: true,
            children: [Some(button), slider].into_iter().flatten().collect(),
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

    fn is_main_window(&self, pid: i32, window_id: u32) -> PortResult<bool> {
        assert_eq!((pid, window_id), (1, 1), "only TextEdit's window can be asked about");
        Ok(!self.not_main)
    }
}

impl UiActions for Fake {
    fn is_at(&self, element: ElementHandle, _point: Point) -> PortResult<bool> {
        Ok((self.is_button(element)? || self.is_slider(element)) && !self.obscured)
    }

    fn frame(&self, element: ElementHandle) -> PortResult<Option<Rect>> {
        if self.is_slider(element) {
            return Ok(self.slider);
        }
        Ok(self.is_button(element)?.then_some(self.button_moved_to.unwrap_or(BUTTON_FRAME)))
    }

    fn perform(&self, element: ElementHandle, action: UiAction) -> PortResult<bool> {
        self.is_button(element)?;
        if self.accessible {
            self.record(format!("ax {action:?} {element}"));
        }
        if self.accessible
            && action == UiAction::Press
            && let Some(state) = self.control.lock().unwrap().as_mut()
            && let LiveValue::Number(now) = state.value
        {
            let on = now != 1.0 || state.role == "AXRadioButton";
            state.value = LiveValue::Number(if on { 1.0 } else { 0.0 });
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

    fn control(&self, element: ElementHandle) -> PortResult<ControlState> {
        self.is_button(element)?;
        let reads = {
            let mut reads = self.control_reads.lock().unwrap();
            *reads += 1;
            *reads
        };
        if self.control_read_once && reads > 1 {
            return Err("the element no longer exists".to_string());
        }
        Ok(self.control.lock().unwrap().clone().unwrap_or(ControlState {
            role: "AXButton".to_string(),
            enabled: true,
            value: LiveValue::None,
            settable: false,
            min: None,
            max: None,
        }))
    }

    fn set_number(&self, element: ElementHandle, value: f64) -> PortResult<()> {
        self.is_button(element)?;
        self.record(format!("ax set {element} {value}"));
        let snapped = self.step.map_or(value, |step| (value / step).round() * step);
        if let Some(state) = self.control.lock().unwrap().as_mut() {
            state.value = LiveValue::Number(snapped);
        }
        Ok(())
    }

    fn choose_option(
        &self,
        element: ElementHandle,
        pick: &dyn Fn(&[MenuOption]) -> Option<usize>,
    ) -> PortResult<MenuChoice> {
        self.is_button(element)?;
        self.record(format!("ax open menu {element}"));
        if self.pause_once_menu_open {
            *self.paused.lock().unwrap() = true;
        }
        let Some(chosen) = pick(&self.options) else {
            self.record(format!("ax cancel menu {element}"));
            return Ok(MenuChoice::Cancelled(self.options.clone()));
        };
        let title = self.options[chosen].title.clone();
        self.record(format!("ax choose {element} {title}"));
        if let Some(state) = self.control.lock().unwrap().as_mut() {
            state.value = LiveValue::Text(title);
        }
        Ok(MenuChoice::Pressed)
    }

    fn menu_items(&self, pid: i32, path: &[MenuStep]) -> PortResult<Option<Vec<MenuItem>>> {
        assert_eq!(pid, 1, "menus must be read of the access-checked app");
        self.record(format!("menu read {}", menu_path(path)));
        if self.pause_once_menu_read {
            *self.paused.lock().unwrap() = true;
        }
        Ok(self.menu_level(path).map(|level| level.iter().map(|menu| menu.item.clone()).collect()))
    }

    fn press_menu_item(&self, pid: i32, path: &[MenuStep]) -> PortResult<bool> {
        assert_eq!(pid, 1, "menus must be pressed in the access-checked app");
        let (last, parents) = path.split_last().expect("a path to press");
        let Some(item) = self.menu_level(parents).and_then(|level| level.get(last.index)) else { return Ok(false) };
        let title = self.renamed_at_press.as_ref().unwrap_or(&item.item.title);
        if title != &last.title || self.kind_at_press.unwrap_or(item.item.kind) != MenuKind::App {
            return Ok(false);
        }
        self.record(format!("menu press {}", menu_path(path)));
        Ok(true)
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

    fn drag(&self, pid: i32, window_id: u32, from: Point, to: Point, stop: &dyn Fn() -> bool) -> PortResult<DragOutcome> {
        assert_eq!((pid, window_id), (1, 1), "real input must name the access-checked window");
        if self.covered {
            return Ok(DragOutcome::NotOnTop);
        }
        self.record(format!("drag {},{} -> {},{}", from.x, from.y, to.x, to.y));
        if self.pause_once_dragging {
            *self.paused.lock().unwrap() = true;
        }
        if stop() {
            self.record("stopped".to_string());
            return Ok(DragOutcome::Cancelled { why: DragCancel::Stopped, undo: self.drag_undo.unwrap_or(DragUndo::AtStart) });
        }
        Ok(self.drag_outcome.unwrap_or(DragOutcome::Dropped))
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

/// A session that has observed window 1 at half scale (1000x500 px); the
/// button is then `e2`.
pub fn observed(fake: Fake) -> (Session, Arc<Fake>) {
    let (session, fake) = session_with(fake);
    let request = ObserveRequest {
        window_id: 1,
        screenshot: Shot::None,
        tree: true,
        reference: None,
        max_edge: 1000,
        filter: Default::default(),
    };
    session.observe(&policy(), request).unwrap();
    (session, fake)
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
