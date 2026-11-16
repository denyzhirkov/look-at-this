//! macOS implementations of the ports.

mod consent;
mod input;
mod keyboard;
mod overlay;
mod pause;
mod permissions;
mod screen;
mod ui_tree;

pub use consent::MacConsent;
pub use input::MacInput;
pub use overlay::MacOverlay;
pub use pause::MacPause;
pub use permissions::MacPermissions;
pub use screen::MacScreen;
pub use ui_tree::MacUiTree;
