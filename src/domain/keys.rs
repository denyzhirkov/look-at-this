//! Parsing key combinations such as `cmd+shift+s`.

use crate::ports::{Key, KeyCombo, Modifiers};

/// Parts are joined with `+`; the last one is the key, the rest modifiers.
/// The `+` key itself is written `plus`.
pub fn parse(combo: &str) -> Result<KeyCombo, String> {
    let lowered = combo.trim().to_lowercase();
    let mut parts: Vec<&str> = lowered.split('+').map(str::trim).collect();
    let key_name = parts.pop().unwrap_or_default();
    let mut modifiers = Modifiers::default();
    for part in parts {
        match part {
            "cmd" | "command" | "meta" => modifiers.cmd = true,
            "ctrl" | "control" => modifiers.ctrl = true,
            "alt" | "option" | "opt" => modifiers.alt = true,
            "shift" => modifiers.shift = true,
            other => return Err(format!("unknown modifier \"{other}\"; use cmd, ctrl, alt, shift")),
        }
    }
    Ok(KeyCombo { modifiers, key: key(key_name)? })
}

fn key(name: &str) -> Result<Key, String> {
    Ok(match name {
        "enter" | "return" => Key::Enter,
        "tab" => Key::Tab,
        "esc" | "escape" => Key::Escape,
        "space" => Key::Space,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "plus" => Key::Char('+'),
        _ => {
            let mut chars = name.chars();
            match (chars.next(), chars.as_str()) {
                (Some(c), "") => Key::Char(c),
                (Some('f'), number) => match number.parse() {
                    Ok(n @ 1..=12) => Key::F(n),
                    _ => return Err(unknown(name)),
                },
                _ => return Err(unknown(name)),
            }
        }
    })
}

fn unknown(name: &str) -> String {
    format!(
        "unknown key \"{name}\"; use one character or enter, tab, escape, space, backspace, \
         delete, up, down, left, right, home, end, pageup, pagedown, plus, f1..f12"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn combo(cmd: bool, shift: bool, key: Key) -> KeyCombo {
        KeyCombo { modifiers: Modifiers { cmd, shift, ..Modifiers::default() }, key }
    }

    #[test]
    fn parses_modifiers_and_keys_in_any_case() {
        assert_eq!(parse("cmd+s"), Ok(combo(true, false, Key::Char('s'))));
        assert_eq!(parse(" Cmd + Shift + S "), Ok(combo(true, true, Key::Char('s'))));
        assert_eq!(parse("Enter"), Ok(combo(false, false, Key::Enter)));
        assert_eq!(parse("shift+F12"), Ok(combo(false, true, Key::F(12))));
        assert_eq!(parse("cmd+plus"), Ok(combo(true, false, Key::Char('+'))));
        assert_eq!(parse("f"), Ok(combo(false, false, Key::Char('f'))));
    }

    #[test]
    fn rejects_what_it_does_not_know() {
        assert!(parse("super+s").is_err());
        assert!(parse("cmd+save").is_err());
        assert!(parse("f13").is_err());
        assert!(parse("cmd+").is_err());
        assert!(parse("").is_err());
    }
}
