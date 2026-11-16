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
}

pub trait WindowSource: Send + Sync {
    /// Ordinary on-screen application windows of every app (no menu bar,
    /// Dock, or other system layers). Filtering by policy is the domain's job.
    fn windows(&self) -> PortResult<Vec<WindowInfo>>;
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

/// One accessibility element. Strings are empty when the attribute is absent.
#[derive(Clone, Debug, Default)]
pub struct UiNode {
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
}
