//! macOS implementations of the ports.

mod input;
mod keyboard;
mod permissions;
mod screen;
mod ui_tree;

pub use input::MacInput;
pub use permissions::MacPermissions;
pub use screen::MacScreen;
pub use ui_tree::MacUiTree;
