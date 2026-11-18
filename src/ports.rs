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
    /// Whether the window is its app's main window, the one the app's menu
    /// bar acts on. Issues no element handles.
    fn is_main_window(&self, pid: i32, window_id: u32) -> PortResult<bool>;
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UiAction {
    /// What a click on the element does.
    Press,
    /// What a right-click does: open its context menu.
    ShowMenu,
}

/// An element's `AXValue` as accessibility reports it now.
#[derive(Clone, Debug, PartialEq)]
pub enum LiveValue {
    /// No value, or one of a kind not listed here.
    None,
    /// A number; a boolean is reported as 0 or 1.
    Number(f64),
    Text(String),
    /// The element is a secure (password) field, so its value was not read.
    Secret,
}

/// What a control is and holds right now, read live rather than from a tree.
#[derive(Clone, Debug, PartialEq)]
pub struct ControlState {
    /// Platform role, e.g. `AXCheckBox`.
    pub role: String,
    pub enabled: bool,
    pub value: LiveValue,
    /// Whether `AXValue` accepts being set.
    pub settable: bool,
    /// `AXMinValue` and `AXMaxValue`, when reported as numbers.
    pub min: Option<f64>,
    pub max: Option<f64>,
}

/// One item of a pop-up button's menu.
#[derive(Clone, Debug, PartialEq)]
pub struct MenuOption {
    /// Empty for a separator.
    pub title: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MenuChoice {
    /// The chosen item was pressed and the menu has closed.
    Pressed,
    /// Nothing was chosen and the menu was closed again; these were its items.
    Cancelled(Vec<MenuOption>),
}

/// Menus the system puts into every app's menu bar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MenuKind {
    /// Any menu or item of the app.
    App,
    /// The Apple menu: the first title of the menu bar.
    Apple,
    /// The item holding the Services submenu, which hands content to other apps.
    Services,
    /// An item of the Services submenu: a service, or the settings for them.
    ServicesItem,
}

/// One item of an app's menu bar or of one of its menus.
#[derive(Clone, Debug, PartialEq)]
pub struct MenuItem {
    /// As the app reports it; empty for a separator.
    pub title: String,
    pub enabled: bool,
    /// The item shows a mark, such as a checkmark.
    pub checked: bool,
    pub has_submenu: bool,
    /// The keyboard shortcut, when it is a printable key (lower case) with
    /// modifiers; `None` when there is none or it is some other key.
    pub shortcut: Option<(Modifiers, char)>,
    pub kind: MenuKind,
    /// Screen points; reported for menu-bar titles only.
    pub frame: Option<Rect>,
}

/// One step down from the menu bar: the item at `index` among the items
/// [`UiActions::menu_items`] lists at that level, which must still carry
/// exactly this title.
#[derive(Clone, Debug, PartialEq)]
pub struct MenuStep {
    pub index: usize,
    pub title: String,
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
    /// Reads the element's role, enabled flag and value now. Never reads the
    /// value of a secure field.
    fn control(&self, element: ElementHandle) -> PortResult<ControlState>;
    /// Sets the element's `AXValue` to a number.
    fn set_number(&self, element: ElementHandle, value: f64) -> PortResult<()>;
    /// Opens a pop-up button's menu and presses the item `pick` chooses among
    /// its items, in menu order; returns `Pressed` once the menu has closed,
    /// so the new value can be read. When `pick` chooses none, or anything
    /// fails once the menu is open, the menu is closed again before
    /// returning: it is never left open.
    fn choose_option(
        &self,
        element: ElementHandle,
        pick: &dyn Fn(&[MenuOption]) -> Option<usize>,
    ) -> PortResult<MenuChoice>;
    /// The items of the menu that `path` leads to from the menu bar of
    /// process `pid` (the menu-bar titles for an empty path), separators
    /// included, in menu order; empty if the last item has no submenu. Read
    /// without opening any menu. `Ok(None)` if an item of `path` is no longer
    /// at its index with its title, or is now of a [`MenuKind`] other than
    /// `App`. Issues no element handles.
    fn menu_items(&self, pid: i32, path: &[MenuStep]) -> PortResult<Option<Vec<MenuItem>>>;
    /// Presses the item `path` leads to, without opening any menu, after
    /// checking every item of `path` still has its title there and is of
    /// kind `App`; `Ok(false)`, with nothing pressed, if one is not.
    fn press_menu_item(&self, pid: i32, path: &[MenuStep]) -> PortResult<bool>;
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

/// How an [`Input::drag`] ended.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DragOutcome {
    /// The button was released at the end point.
    Dropped,
    /// The window was not the topmost window at the start point: nothing
    /// was pressed.
    NotOnTop,
    /// A point of the drag's path is not on any display: nothing was pressed.
    OffDisplay,
    /// The drag was cancelled instead of dropped.
    Cancelled { why: DragCancel, undo: DragUndo },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DragCancel {
    /// The window was no longer the topmost window at a point of the path,
    /// or at the end no longer at the frame it had at the press.
    Covered,
    /// `stop` said so between two steps.
    Stopped,
    /// The pointer was not where the last step had put it: the user moved
    /// the mouse.
    PointerMoved,
}

/// Where a cancelled drag released the button. What is dragged by
/// drag-and-drop lands on whatever is under the pointer at the release.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DragUndo {
    /// Back at the start point, after retracing the path.
    AtStart,
    /// The way back to the start was covered, so at the point of the path
    /// nearest the start that the pointer reached back over points where the
    /// window was still the topmost window, itself such a point.
    OnPath,
    /// The point right behind the pointer was covered, or the release point
    /// was by the time the pointer got there: Escape went to the window's
    /// process, to cancel a drag-and-drop, before the release where the
    /// pointer was. An app that is not in a drag-and-drop gets that Escape
    /// as an ordinary key press.
    Escaped,
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
    /// Presses the left button at `from`, moves the real pointer to `to` in
    /// small steps, releases it there, then puts the pointer back; about
    /// 1.1 s in all.
    ///
    /// Every point of the path must be on a display, or nothing is pressed
    /// ([`DragOutcome::OffDisplay`]). The press follows the rule of
    /// [`Input::click`]: [`DragOutcome::NotOnTop`], with nothing pressed,
    /// unless `window_id` is the topmost window at `from` before the pointer
    /// moves and again right before the press. After each step `stop` is
    /// asked, the pointer must still be where the step put it, and
    /// `window_id` must be the topmost window there. Right before the release
    /// it must still be the topmost window at `to` and at the frame it had at
    /// the press. When any of that fails the drag is cancelled, not dropped:
    /// the pointer retraces its path, only over points where `window_id` is
    /// the topmost window now, and the button is released at the one of them
    /// nearest `from`. If the point right behind the pointer is not one, or
    /// the release point no longer is when the pointer gets there, Escape is
    /// sent to process `pid` and the button is released where the pointer is
    /// ([`DragUndo`]).
    ///
    /// Every return releases the button. Nothing does when the process dies
    /// mid-drag (it aborts on a panic; it can be killed): the system then
    /// keeps the button down until the user's next click. An error means
    /// nothing was pressed.
    fn drag(
        &self,
        pid: i32,
        window_id: u32,
        from: Point,
        to: Point,
        stop: &dyn Fn() -> bool,
    ) -> PortResult<DragOutcome>;
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
