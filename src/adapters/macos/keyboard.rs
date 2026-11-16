//! Which virtual key to press for a key combination, in the keyboard layout
//! the user has, not a US one.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_foundation::{CFData, CFRetained, CFString, CFType};
use objc2_core_graphics::CGKeyCode;

use crate::ports::{Key, KeyCombo, PortResult};

pub(super) const RETURN_KEY: CGKeyCode = 36;
/// Virtual key codes of the character block of a Mac keyboard (letters,
/// digits, punctuation); the keypad and the named keys come after it.
const CHARACTER_KEYS: std::ops::RangeInclusive<CGKeyCode> = 0..=50;
/// `shiftKey >> 8`, the modifier state UCKeyTranslate takes.
const SHIFT_STATE: u32 = 0x02;
const KEY_ACTION_DOWN: u16 = 0;
const NO_DEAD_KEYS: u32 = 1;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    /// +1 reference (Copy rule), or NULL.
    fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> Option<NonNull<CFType>>;
    /// Not retained: owned by `source`.
    fn TISGetInputSourceProperty(source: &CFType, key: &CFString) -> *const CFData;
    static kTISPropertyUnicodeKeyLayoutData: &'static CFString;
    fn LMGetKbdType() -> u8;
    fn UCKeyTranslate(
        layout: *const c_void,
        key_code: u16,
        key_action: u16,
        modifier_state: u32,
        keyboard_type: u32,
        options: u32,
        dead_key_state: *mut u32,
        max_length: usize,
        actual_length: *mut usize,
        unicode: *mut u16,
    ) -> i32;
}

/// What one key types, without and with Shift.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct KeyCap {
    pub code: CGKeyCode,
    pub plain: Option<char>,
    pub shifted: Option<char>,
}

/// One key press: its virtual key, whether Shift is held, and the character
/// to attach to the event, if any.
#[derive(Debug, PartialEq)]
pub(super) struct Press {
    pub code: CGKeyCode,
    pub shift: bool,
    pub text: Option<char>,
}

/// The keys of the layout macOS matches shortcuts against: the current
/// ASCII-capable one (the user's own layout when it is Latin, else e.g. ABC).
///
/// Main thread only: Text Input Sources are documented to require it, and
/// macOS 26 crash reports show them aborting the process otherwise.
pub(super) fn shortcut_layout() -> PortResult<Vec<KeyCap>> {
    const UNAVAILABLE: &str = "macOS did not provide the current keyboard layout (Text Input \
         Sources returned nothing)";
    // SAFETY: returns a +1 reference or NULL.
    let source = unsafe { TISCopyCurrentASCIICapableKeyboardLayoutInputSource() }
        .map(|source| unsafe { CFRetained::from_raw(source) })
        .ok_or(UNAVAILABLE)?;
    // SAFETY: a valid input source and a Carbon constant; the data lives as long as `source`.
    let data = unsafe { TISGetInputSourceProperty(&source, kTISPropertyUnicodeKeyLayoutData).as_ref() }
        .ok_or(UNAVAILABLE)?;
    // SAFETY: Carbon call without arguments.
    let keyboard_type = u32::from(unsafe { LMGetKbdType() });
    let layout = data.byte_ptr().cast::<c_void>();
    let translate = |code: CGKeyCode, modifiers: u32| {
        let mut dead_keys = 0;
        let mut length = 0;
        let mut unicode = [0u16; 4];
        // SAFETY: `layout` is 'uchr' data kept alive by `source`; every
        // out-pointer is valid and `unicode` holds `max_length` units.
        let status = unsafe {
            UCKeyTranslate(
                layout,
                code,
                KEY_ACTION_DOWN,
                modifiers,
                keyboard_type,
                NO_DEAD_KEYS,
                &mut dead_keys,
                unicode.len(),
                &mut length,
                unicode.as_mut_ptr(),
            )
        };
        let mut chars = char::decode_utf16(unicode[..length.min(unicode.len())].iter().copied());
        match (status, chars.next(), chars.next()) {
            (0, Some(Ok(c)), None) if !c.is_control() => Some(c),
            _ => None,
        }
    };
    Ok(CHARACTER_KEYS
        .map(|code| KeyCap { code, plain: translate(code, 0), shifted: translate(code, SHIFT_STATE) })
        .collect())
}

/// The key that types `c` in `layout`, and whether Shift is needed for it.
/// A key that types it without Shift wins.
fn find(layout: &[KeyCap], c: char) -> Option<(KeyCap, bool)> {
    let plain = layout.iter().find(|cap| cap.plain == Some(c)).map(|cap| (*cap, false));
    plain.or_else(|| layout.iter().find(|cap| cap.shifted == Some(c)).map(|cap| (*cap, true)))
}

/// How to press `combo` on a keyboard with `layout`.
pub(super) fn press(combo: KeyCombo, layout: &[KeyCap]) -> PortResult<Press> {
    let modifiers = combo.modifiers;
    let code = match combo.key {
        Key::Char(c) => {
            let (cap, needs_shift) = find(layout, c).ok_or_else(|| {
                format!("no key of the keyboard layout types \"{c}\"; use type_text to enter it as text")
            })?;
            let shift = modifiers.shift || needs_shift;
            // A key that types carries its character, so the user's current
            // layout (e.g. Russian) cannot change it. A shortcut must look
            // exactly like the physical key instead: with the character
            // attached, TextEdit no longer matched cmd+a to Select All (seen live).
            let shortcut = modifiers.cmd || modifiers.ctrl || modifiers.alt;
            let typed = if shift { cap.shifted } else { cap.plain };
            return Ok(Press { code: cap.code, shift, text: typed.filter(|_| !shortcut) });
        }
        Key::Enter => RETURN_KEY,
        Key::Tab => 48,
        Key::Space => 49,
        Key::Backspace => 51,
        Key::Escape => 53,
        Key::Home => 115,
        Key::PageUp => 116,
        Key::Delete => 117,
        Key::End => 119,
        Key::PageDown => 121,
        Key::Left => 123,
        Key::Right => 124,
        Key::Down => 125,
        Key::Up => 126,
        Key::F(n) => match n {
            1 => 122,
            2 => 120,
            3 => 99,
            4 => 118,
            5 => 96,
            6 => 97,
            7 => 98,
            8 => 100,
            9 => 101,
            10 => 109,
            11 => 103,
            12 => 111,
            _ => return Err(format!("there is no key F{n}; use f1 to f12")),
        },
    };
    Ok(Press { code, shift: modifiers.shift, text: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::Modifiers;

    fn cap(code: CGKeyCode, plain: char, shifted: char) -> KeyCap {
        KeyCap { code, plain: Some(plain), shifted: Some(shifted) }
    }

    /// A few keys of French AZERTY: A and Q swapped, digits need Shift.
    fn azerty() -> Vec<KeyCap> {
        vec![cap(0, 'q', 'Q'), cap(12, 'a', 'A'), cap(18, '&', '1'), cap(24, '=', '+'), cap(44, '!', '§')]
    }

    fn combo(key: Key, cmd: bool, shift: bool) -> KeyCombo {
        KeyCombo { modifiers: Modifiers { cmd, shift, ..Modifiers::default() }, key }
    }

    #[test]
    fn shortcuts_use_the_key_of_the_users_layout() {
        let cmd_a = press(combo(Key::Char('a'), true, false), &azerty());
        assert_eq!(cmd_a, Ok(Press { code: 12, shift: false, text: None }), "not the US position, 0 (Q here)");
        let cmd_plus = press(combo(Key::Char('+'), true, false), &azerty());
        assert_eq!(cmd_plus, Ok(Press { code: 24, shift: true, text: None }));
    }

    #[test]
    fn typing_keys_carry_the_character_shift_included() {
        let layout = azerty();
        assert_eq!(press(combo(Key::Char('a'), false, false), &layout), Ok(Press { code: 12, shift: false, text: Some('a') }));
        assert_eq!(press(combo(Key::Char('a'), false, true), &layout), Ok(Press { code: 12, shift: true, text: Some('A') }));
        assert_eq!(press(combo(Key::Char('1'), false, false), &layout), Ok(Press { code: 18, shift: true, text: Some('1') }));
    }

    #[test]
    fn named_keys_keep_their_modifiers_and_carry_no_character() {
        assert_eq!(press(combo(Key::Tab, false, true), &[]), Ok(Press { code: 48, shift: true, text: None }));
        assert_eq!(press(combo(Key::F(12), false, false), &[]), Ok(Press { code: 111, shift: false, text: None }));
    }

    #[test]
    fn characters_no_key_types_are_refused() {
        assert!(press(combo(Key::Char('ж'), true, false), &azerty()).is_err());
        assert!(press(combo(Key::Char('a'), true, false), &[]).is_err());
        assert!(press(combo(Key::F(13), false, false), &[]).is_err());
    }
}
