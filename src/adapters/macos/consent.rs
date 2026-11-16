//! The system dialog that asks the user to approve allowlisting an app.
//!
//! A CFUserNotification alert: the UserNotificationCenter system process
//! draws and runs it, so this short-lived command needs no event loop of its
//! own, and the window belongs to an app agents may never control (policy.rs).

use objc2_app_kit::NSWorkspace;
use objc2_core_foundation::{
    CFOptionFlags, CFString, CFUserNotification, kCFUserNotificationAlternateResponse,
    kCFUserNotificationCautionAlertLevel,
};
use objc2_foundation::{NSFileManager, NSString};

use crate::ports::{Consent, PortResult};

/// After this long without an answer the dialog closes and counts as a no.
const TIMEOUT_SECS: f64 = 120.0;

pub struct MacConsent;

impl Consent for MacConsent {
    fn confirm_allow(&self, bundle_id: &str) -> PortResult<bool> {
        let (header, message) = dialog_text(bundle_id, installed_app_name(bundle_id).as_deref());
        let mut response: CFOptionFlags = 0;
        // The default button, which Return picks, is Don't Allow; Allow is the
        // alternate one. The call blocks until a button is clicked or the timeout.
        // SAFETY: every string is a valid CFString; `response` outlives the call.
        let status = unsafe {
            CFUserNotification::display_alert(
                TIMEOUT_SECS,
                kCFUserNotificationCautionAlertLevel,
                None,
                None,
                None,
                Some(&CFString::from_str(&header)),
                Some(&CFString::from_str(&message)),
                Some(&CFString::from_static_str("Don't Allow")),
                Some(&CFString::from_static_str("Allow")),
                None,
                &mut response,
            )
        };
        answer(status, response)
    }
}

/// Only a click on Allow approves; a timeout comes back as the Cancel response.
fn answer(status: i32, response: CFOptionFlags) -> PortResult<bool> {
    if status != 0 {
        return Err(format!(
            "cannot show the confirmation dialog (error {status}); \
             run `look-at-this allow` in your logged-in desktop session"
        ));
    }
    // The low two bits name the button; the others carry checkbox states.
    Ok(response & 0b11 == kCFUserNotificationAlternateResponse)
}

/// The dialog's header and message. Always shows the bundle id itself, since
/// that is what the allowlist stores.
fn dialog_text(bundle_id: &str, app_name: Option<&str>) -> (String, String) {
    let effect = "AI agents using look-at-this will be able to see its windows and click and type in it.";
    match app_name {
        Some(name) => (
            format!("Allow AI agents to control “{name}”?"),
            format!("{effect}\n\nBundle id: {bundle_id}"),
        ),
        None => (
            "Allow AI agents to control an app that is not installed?".to_string(),
            format!("No installed app with this bundle id was found.\n\n{effect}\n\nBundle id: {bundle_id}"),
        ),
    }
}

/// The name Finder shows for the installed app with this bundle id.
fn installed_app_name(bundle_id: &str) -> Option<String> {
    let url = NSWorkspace::sharedWorkspace().URLForApplicationWithBundleIdentifier(&NSString::from_str(bundle_id))?;
    let path = url.path()?;
    Some(NSFileManager::defaultManager().displayNameAtPath(&path).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_allow_approves() {
        assert_eq!(answer(0, 1), Ok(true));
        // Allow with a checkbox bit set still names the Allow button.
        assert_eq!(answer(0, 1 | 1 << 8), Ok(true));
        // Don't Allow (the default button) and the timeout's Cancel.
        assert_eq!(answer(0, 0), Ok(false));
        assert_eq!(answer(0, 3), Ok(false));
        assert!(answer(-1, 1).is_err());
    }

    #[test]
    fn the_dialog_always_shows_the_bundle_id() {
        let (header, message) = dialog_text("com.apple.TextEdit", Some("TextEdit"));
        assert!(header.contains("TextEdit"));
        assert!(message.contains("Bundle id: com.apple.TextEdit"));
        let (_, message) = dialog_text("local.unknown", None);
        assert!(message.contains("No installed app"));
        assert!(message.contains("Bundle id: local.unknown"));
    }
}
