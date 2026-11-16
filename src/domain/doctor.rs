//! Health report: is the server able to see and act, and on what.

use std::path::Path;

use serde::Serialize;

use crate::config::Config;
use crate::domain::policy::{Access, AccessPolicy};
use crate::ports::Permissions;

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub screen_recording: bool,
    pub accessibility: bool,
    pub config_path: String,
    pub allowed_apps: Vec<String>,
    /// True while the user's emergency stop blocks all agent input.
    pub input_paused: bool,
    /// What the user has to fix, in plain words. Empty when ready.
    pub problems: Vec<String>,
}

pub fn report(
    permissions: &dyn Permissions,
    config_path: &Path,
    config: Result<Config, String>,
    input_paused: bool,
) -> DoctorReport {
    let screen_recording = permissions.screen_recording();
    let accessibility = permissions.accessibility();
    let mut problems = Vec::new();

    if !screen_recording {
        problems.push(permission_problem("Screen & System Audio Recording"));
    }
    if !accessibility {
        problems.push(permission_problem("Accessibility"));
    }

    if input_paused {
        problems.push(
            "The user has paused agent input with the emergency stop (ctrl+option+cmd+escape). \
             Clicks and typing are refused until the user presses it again."
                .to_string(),
        );
    }

    let mut allowed_apps = Vec::new();
    match config {
        Err(error) => problems.push(format!("Config is unreadable: {error}")),
        Ok(config) => {
            let policy = AccessPolicy::new(&config.allowed_apps);
            for app in config.allowed_apps.iter() {
                match policy.check(app) {
                    Access::Forbidden => problems.push(format!(
                        "{app} is in the allowlist but can never be controlled; it is ignored."
                    )),
                    _ => allowed_apps.push(app.clone()),
                }
            }
            if allowed_apps.is_empty() {
                problems.push(
                    "No apps are allowed. The user must run `look-at-this allow <bundle-id>`."
                        .to_string(),
                );
            }
        }
    }

    DoctorReport {
        screen_recording,
        accessibility,
        config_path: config_path.display().to_string(),
        allowed_apps,
        input_paused,
        problems,
    }
}

fn permission_problem(pane: &str) -> String {
    format!(
        "{pane} permission is missing. macOS grants it to the app that launched this server \
         (the terminal or agent host): System Settings → Privacy & Security → {pane}, \
         then restart that app."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        screen_recording: bool,
        accessibility: bool,
    }

    impl Permissions for Fake {
        fn screen_recording(&self) -> bool {
            self.screen_recording
        }
        fn accessibility(&self) -> bool {
            self.accessibility
        }
    }

    fn config(apps: &[&str]) -> Result<Config, String> {
        Ok(Config {
            allowed_apps: apps.iter().map(|s| s.to_string()).collect(),
        })
    }

    #[test]
    fn ready_setup_has_no_problems() {
        let perms = Fake { screen_recording: true, accessibility: true };
        let r = report(&perms, Path::new("/c.toml"), config(&["com.apple.TextEdit"]), false);
        assert!(r.problems.is_empty(), "{:?}", r.problems);
        assert_eq!(r.allowed_apps, ["com.apple.TextEdit"]);
    }

    #[test]
    fn every_gap_is_reported() {
        let perms = Fake { screen_recording: false, accessibility: false };
        let r = report(&perms, Path::new("/c.toml"), config(&["com.apple.Terminal"]), false);
        // Two permissions, one forbidden entry, and nothing usable left.
        assert_eq!(r.problems.len(), 4, "{:?}", r.problems);
        assert!(r.allowed_apps.is_empty());
    }

    #[test]
    fn broken_config_is_a_problem_not_a_crash() {
        let perms = Fake { screen_recording: true, accessibility: true };
        let r = report(&perms, Path::new("/c.toml"), Err("bad toml".to_string()), true);
        // The unreadable config and the emergency stop.
        assert_eq!(r.problems.len(), 2);
        assert!(r.input_paused);
        assert!(r.problems.iter().any(|p| p.contains("bad toml")));
    }
}
