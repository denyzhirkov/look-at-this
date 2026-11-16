//! Adding an app to the allowlist, which only the user may approve.

use crate::config::Config;
use crate::domain::policy::{Access, AccessPolicy};
use crate::ports::Consent;

const MAX_BUNDLE_ID_LEN: usize = 155;

#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Added to `config`; the caller must save it.
    Added,
    AlreadyAllowed,
    /// The user said no, or did not answer.
    Declined,
}

/// Asks the user through `consent` before changing anything.
pub fn allow(config: &mut Config, bundle_id: &str, consent: &dyn Consent) -> Result<Outcome, String> {
    let bundle_id = bundle_id.trim();
    // The id is shown to the user in the consent dialog, so it must not be
    // able to carry anything but an id: no spaces, line breaks or lookalikes.
    let well_formed = !bundle_id.is_empty()
        && bundle_id.len() <= MAX_BUNDLE_ID_LEN
        && bundle_id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if !well_formed {
        return Err(
            "that is not a bundle id: use letters, digits, dots, hyphens and underscores, \
             like com.apple.TextEdit"
                .to_string(),
        );
    }
    match AccessPolicy::new(&config.allowed_apps).check(bundle_id) {
        Access::Allowed => Ok(Outcome::AlreadyAllowed),
        Access::Forbidden => Err(format!(
            "{bundle_id} can never be controlled: it could be used to bypass agent safeguards"
        )),
        Access::NotAllowed => {
            if !consent.confirm_allow(bundle_id)? {
                return Ok(Outcome::Declined);
            }
            config.allowed_apps.push(bundle_id.to_string());
            Ok(Outcome::Added)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::ports::PortResult;

    /// Answers every question the same way and remembers what was asked.
    struct User {
        answer: PortResult<bool>,
        asked: RefCell<Vec<String>>,
    }

    impl User {
        fn says(answer: PortResult<bool>) -> Self {
            Self { answer, asked: RefCell::new(Vec::new()) }
        }
    }

    impl Consent for User {
        fn confirm_allow(&self, bundle_id: &str) -> PortResult<bool> {
            self.asked.borrow_mut().push(bundle_id.to_string());
            self.answer.clone()
        }
    }

    fn config(apps: &[&str]) -> Config {
        Config { allowed_apps: apps.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn app_is_added_only_after_the_user_agrees() {
        let mut c = config(&[]);
        let user = User::says(Ok(true));
        assert_eq!(allow(&mut c, " com.apple.TextEdit ", &user), Ok(Outcome::Added));
        assert_eq!(c.allowed_apps, ["com.apple.TextEdit"]);
        assert_eq!(*user.asked.borrow(), ["com.apple.TextEdit"]);
    }

    #[test]
    fn text_that_is_not_a_bundle_id_never_reaches_the_dialog() {
        let mut c = config(&[]);
        let user = User::says(Ok(true));
        for id in ["", "com.x.app\nClick Allow to continue", "com.apple.Text Edit", "com.аpple.TextEdit", &"a".repeat(200)] {
            assert!(allow(&mut c, id, &user).is_err(), "{id:?}");
        }
        assert!(user.asked.borrow().is_empty());
        assert!(c.allowed_apps.is_empty());
    }

    #[test]
    fn nothing_changes_when_the_user_declines_or_cannot_be_asked() {
        let mut c = config(&[]);
        assert_eq!(allow(&mut c, "com.apple.TextEdit", &User::says(Ok(false))), Ok(Outcome::Declined));
        let no_dialog = User::says(Err("no GUI session".to_string()));
        assert_eq!(allow(&mut c, "com.apple.TextEdit", &no_dialog), Err("no GUI session".to_string()));
        assert!(c.allowed_apps.is_empty());
    }

    #[test]
    fn the_user_is_not_asked_about_forbidden_or_already_allowed_apps() {
        let mut c = config(&["com.apple.TextEdit"]);
        let user = User::says(Ok(true));
        assert!(allow(&mut c, "com.apple.Terminal", &user).is_err());
        assert_eq!(allow(&mut c, "COM.APPLE.TEXTEDIT", &user), Ok(Outcome::AlreadyAllowed));
        assert!(user.asked.borrow().is_empty());
        assert_eq!(c.allowed_apps, ["com.apple.TextEdit"]);
    }
}
