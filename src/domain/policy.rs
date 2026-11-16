//! Which applications the agent may see and control.

/// Apps the agent must never control, even if the user allowlists them.
///
/// Terminals, editors with an integrated terminal and the agent's own host
/// would let the agent type commands that bypass its permission prompts.
/// System Settings would let it grant itself more access; password stores
/// hold secrets.
const FORBIDDEN_APPS: &[&str] = &[
    "com.apple.terminal",
    "com.googlecode.iterm2",
    "dev.warp.warp-stable",
    "com.mitchellh.ghostty",
    "net.kovidgoyal.kitty",
    "org.alacritty",
    "com.github.wez.wezterm",
    "co.zeit.hyper",
    "com.microsoft.vscode",
    "com.todesktop.230313mzl4w4u92", // Cursor
    "com.microsoft.vscodeinsiders",
    "com.exafunction.windsurf",
    "dev.zed.zed",
    "com.anthropic.claudefordesktop",
    "com.apple.scripteditor2",
    "com.apple.automator",
    "com.apple.shortcuts",
    "com.apple.systempreferences",
    "com.apple.securityagent",
    "com.apple.keychainaccess",
    "com.apple.passwords",
    "com.1password.1password",
    "com.bitwarden.desktop",
];

/// Whole vendors whose apps all embed a terminal (JetBrains IDEs).
const FORBIDDEN_PREFIXES: &[&str] = &["com.jetbrains."];

#[derive(Debug, PartialEq, Eq)]
pub enum Access {
    Allowed,
    /// Not on the user's allowlist; the user can add it.
    NotAllowed,
    /// On the hard denylist; cannot be allowed.
    Forbidden,
}

pub struct AccessPolicy {
    allowed: Vec<String>,
}

impl AccessPolicy {
    pub fn new<'a>(allowed_apps: impl IntoIterator<Item = &'a String>) -> Self {
        Self {
            allowed: allowed_apps.into_iter().map(|id| normalize(id)).collect(),
        }
    }

    /// Bundle ids are compared case-insensitively, as macOS does.
    pub fn check(&self, bundle_id: &str) -> Access {
        let id = normalize(bundle_id);
        if FORBIDDEN_APPS.contains(&id.as_str()) || FORBIDDEN_PREFIXES.iter().any(|p| id.starts_with(p)) {
            Access::Forbidden
        } else if self.allowed.contains(&id) {
            Access::Allowed
        } else {
            Access::NotAllowed
        }
    }
}

fn normalize(bundle_id: &str) -> String {
    bundle_id.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(ids: &[&str]) -> AccessPolicy {
        let owned: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        AccessPolicy::new(&owned)
    }

    #[test]
    fn allowlisted_app_is_allowed_regardless_of_case() {
        let p = policy(&["com.apple.TextEdit"]);
        assert_eq!(p.check("com.apple.TextEdit"), Access::Allowed);
        assert_eq!(p.check("COM.APPLE.TEXTEDIT"), Access::Allowed);
    }

    #[test]
    fn unlisted_app_is_not_allowed() {
        let p = policy(&["com.apple.TextEdit"]);
        assert_eq!(p.check("com.apple.Notes"), Access::NotAllowed);
        assert_eq!(policy(&[]).check("com.apple.TextEdit"), Access::NotAllowed);
    }

    #[test]
    fn forbidden_app_stays_forbidden_even_when_allowlisted() {
        let p = policy(&["com.apple.Terminal", "com.apple.systempreferences"]);
        assert_eq!(p.check("com.apple.Terminal"), Access::Forbidden);
        assert_eq!(p.check("com.apple.SystemPreferences"), Access::Forbidden);
        assert_eq!(policy(&[]).check("com.googlecode.iterm2"), Access::Forbidden);
        assert_eq!(policy(&[]).check("com.jetbrains.intellij"), Access::Forbidden);
    }
}
