//! macOS implementations of the ports.

use crate::ports::Permissions;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    safe fn CGPreflightScreenCaptureAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Returns a `Boolean` (unsigned char).
    safe fn AXIsProcessTrusted() -> u8;
}

pub struct MacPermissions;

impl Permissions for MacPermissions {
    fn screen_recording(&self) -> bool {
        CGPreflightScreenCaptureAccess()
    }

    fn accessibility(&self) -> bool {
        AXIsProcessTrusted() != 0
    }
}
