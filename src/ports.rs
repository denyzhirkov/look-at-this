//! Contracts the domain needs from the operating system.
//!
//! Errors are plain messages: the domain never branches on them, it only
//! relays them to the agent.

pub type PortResult<T> = Result<T, String>;

/// A rectangle in points, origin at the top-left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A position in screen points, same space as [`WindowInfo::frame`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// System permissions this process currently holds.
pub trait Permissions: Send + Sync {
    fn screen_recording(&self) -> bool;
    fn accessibility(&self) -> bool;
}

#[derive(Clone, Debug)]
pub struct WindowInfo {
    pub id: u32,
    pub pid: i32,
    pub bundle_id: Option<String>,
    pub app_name: String,
    pub title: String,
    /// Screen points, origin at the top-left of the main display.
    pub frame: Rect,
    /// False when the window is minimised, its app is hidden, or it is on
    /// another Space or in a hidden Stage Manager strip.
    pub on_screen: bool,
}

pub trait WindowSource: Send + Sync {
    /// Ordinary application windows of every app (no menu bar, Dock, or
    /// other system layers): those on screen, and those with a title that are
    /// not (apps keep untitled helper windows off screen). Filtering by
    /// policy is the domain's job.
    fn windows(&self) -> PortResult<Vec<WindowInfo>>;
    /// Process id of the frontmost app; `None` if it cannot be told.
    fn frontmost_pid(&self) -> Option<i32>;
}

pub trait ScreenCapture: Send + Sync {
    /// Captures `region` of the window (points relative to the window's
    /// top-left) scaled to exactly `width` x `height` pixels, as PNG bytes.
    /// Only that window's content is captured, even if it is covered.
    ///
    /// Fails unless the window still belongs to process `pid`: window ids can
    /// be reused, and the caller's access check was made for that process.
    fn capture(&self, window_id: u32, pid: i32, region: Rect, width: u32, height: u32) -> PortResult<Vec<u8>>;
}

/// Identifies a live element of the most recent [`UiTree::tree`] call.
/// Handles of earlier calls are rejected, never resolved to another element.
pub type ElementHandle = u64;

/// One accessibility element. Strings are empty when the attribute is absent.
#[derive(Clone, Debug, Default)]
pub struct UiNode {
    /// Names this element in [`UiActions`] calls.
    pub handle: ElementHandle,
    /// Platform role, e.g. `AXButton`.
    pub role: String,
    pub title: String,
    pub description: String,
    /// Textual value. Never filled for secure (password) fields.
    pub value: String,
    /// Screen points, same space as [`WindowInfo::frame`].
    pub frame: Option<Rect>,
    pub enabled: bool,
    pub focused: bool,
    pub children: Vec<UiNode>,
}

pub struct UiTreeSnapshot {
    pub root: UiNode,
    /// True if traversal stopped at `max_nodes`.
    pub truncated: bool,
}

pub trait UiTree: Send + Sync {
    /// Accessibility tree of one window, reading at most `max_nodes` elements.
    fn tree(&self, pid: i32, window_id: u32, max_nodes: usize) -> PortResult<UiTreeSnapshot>;
    /// The window's frame as accessibility reports it now, in the same space
    /// as [`WindowInfo::frame`]; `None` if it reports none. Issues no element
    /// handles, so those of the latest [`UiTree::tree`] stay valid.
    fn window_frame(&self, pid: i32, window_id: u32) -> PortResult<Option<Rect>>;
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UiAction {
    /// What a click on the element does.
    Press,
    /// What a right-click does: open its context menu.
    ShowMenu,
}

/// Acting on elements through accessibility: no pointer, no focus change.
pub trait UiActions: Send + Sync {
    /// Whether an accessibility hit test at `point` finds this element or
    /// something inside it, i.e. the element is really what is shown there.
    fn is_at(&self, element: ElementHandle, point: Point) -> PortResult<bool>;
    /// The part of the element that is visible now: its current frame cut
    /// down to the scroll areas that enclose it. `None` if it has no frame or
    /// is entirely scrolled out of view.
    fn frame(&self, element: ElementHandle) -> PortResult<Option<Rect>>;
    /// `Ok(false)` if the element does not offer this action.
    fn perform(&self, element: ElementHandle, action: UiAction) -> PortResult<bool>;
    /// Inserts `text` at the element's insertion point, or replaces its whole
    /// content. `Ok(false)` if the element does not accept text this way.
    fn insert_text(&self, element: ElementHandle, text: &str, replace: bool) -> PortResult<bool>;
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Button {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Key {
    /// A printable key, lower case, e.g. `s` or `/`.
    Char(char),
    Enter,
    Tab,
    Escape,
    Space,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    /// F1 to F12.
    F(u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Modifiers {
    pub cmd: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyCombo {
    pub modifiers: Modifiers,
    pub key: Key,
}

/// Real input, as a person would produce it.
pub trait Input: Send + Sync {
    /// Brings the app, and that window of it, to the front. Returns once it
    /// is frontmost (at once if it already is); fails if it does not get there.
    fn activate(&self, pid: i32, window_id: u32) -> PortResult<()>;
    /// Clicks at `point` with the real pointer, then puts the pointer back.
    ///
    /// Returns `Ok(false)` and stops clicking unless `window_id` is the topmost
    /// window at that point, counting every app and system layer (menus,
    /// Dock, banners, overlays). That is checked before the pointer moves and
    /// again right before each button press, since moving the pointer can
    /// itself reveal something (Dock, menu bar, hover popovers). For a double
    /// click refused at its second press, the first click has already landed
    /// on the window.
    fn click(&self, window_id: u32, point: Point, button: Button, count: u8) -> PortResult<bool>;
    /// Scrolls at `point` by lines (positive `dy` reveals content further
    /// down, positive `dx` further right), then puts the pointer back.
    /// Same topmost-window rule and `Ok(false)` as [`Input::click`].
    fn scroll(&self, window_id: u32, point: Point, dx: i32, dy: i32) -> PortResult<bool>;
    /// Types text into the focused element of process `pid`. The keys go to
    /// that process only, never to another app or to system-wide shortcuts.
    fn type_text(&self, pid: i32, text: &str) -> PortResult<()>;
    /// Presses a key combination, delivered like [`Input::type_text`].
    fn press_key(&self, pid: i32, combo: KeyCombo) -> PortResult<()>;
}

/// The agent's own cursor, drawn over the screen so the user can see where
/// the agent is acting. It is only a picture: it never receives input and
/// never appears in screenshots the agent gets.
pub trait Overlay: Send + Sync {
    /// Shows the cursor acting at a screen point. Best effort: never blocks
    /// for long and never fails the action it accompanies.
    fn point_at(&self, point: Point);
}

/// The user's emergency stop for everything the agent does to their apps.
pub trait Pause: Send + Sync {
    /// True while the user has agent input paused.
    fn is_paused(&self) -> bool;
}

/// Asking the user, outside the agent's reach, to approve a change of access.
pub trait Consent {
    /// Shows the user a system dialog asking whether agents may see and
    /// control the app with this bundle id. `Ok(false)` if they decline or
    /// do not answer; an error if no dialog can be shown.
    fn confirm_allow(&self, bundle_id: &str) -> PortResult<bool>;
}
