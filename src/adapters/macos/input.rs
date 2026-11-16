//! Real pointer and keyboard input via CGEvent, plus what it relies on:
//! bringing a window to the front and asking which window is at a point.
//!
//! Calls arrive on background threads, off the AppKit run loop, where
//! NSWorkspace and NSRunningApplication state may be stale. Everything below
//! asks the window server or the target app directly at the moment of the call.

use std::thread::sleep;
use std::time::{Duration, Instant};

use objc2_application_services::{AXError, AXUIElement};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType, CGPoint, CGRect};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventType, CGKeyCode, CGMainDisplayID,
    CGMouseButton, CGRectMakeWithDictionaryRepresentation, CGScrollEventUnit, CGWindowListCopyWindowInfo,
    CGWindowListOption, kCGWindowBounds, kCGWindowIsOnscreen,
};

use super::{keyboard, ui_tree};
use crate::ports::{Button, Input, KeyCombo, Modifiers, Point, PortResult, Rect};

const ACTIVATE_TIMEOUT: Duration = Duration::from_secs(3);
const ACTIVATE_POLL: Duration = Duration::from_millis(30);
/// Between pointer events, so apps see a hover and then a press, as from a person.
const POINTER_PAUSE: Duration = Duration::from_millis(15);
/// After the pointer arrives, before the next topmost check: long enough for
/// what a hover shows at once (a popup window took about two frames live)
/// to be on screen; delayed reveals such as the Dock's are not waited for.
const HOVER_SETTLE: Duration = Duration::from_millis(60);
/// After each typed character. Unpaced, keys were posted at about 17,000 a
/// second while the test app consumed 300 to 600, so thousands queued up and
/// kept arriving for 30 s after the emergency stop. At about 200 a second
/// the app keeps up, and a stop between the domain's portions takes effect at once.
const KEY_PACE: Duration = Duration::from_millis(5);
const EVENT_FAILED: &str = "CoreGraphics could not create an input event; nothing was sent; try again";

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// Private but long-stable (re-exported from SkyLight): this process's
    /// window server connection.
    fn CGSMainConnectionID() -> i32;
    /// Private but long-stable: the window server's own hit test, the one
    /// that decides which window a click goes to. AppKit's public
    /// `windowNumberAtPoint:` gave the same answers live, but only in a
    /// process with an NSApplication. The three undocumented arguments take
    /// the values open-source window managers pass (0, 1, 0).
    fn CGSFindWindowAndOwner(
        connection: i32,
        undocumented_a: u32,
        undocumented_b: i32,
        undocumented_c: i32,
        screen_point: *mut CGPoint,
        window_point: *mut CGPoint,
        window: *mut u32,
        owner_connection: *mut i32,
    ) -> i32;
}

pub struct MacInput {
    /// Read once at startup, on the main thread (see
    /// `keyboard::shortcut_layout`), so a layout the user switches to later
    /// takes effect only after the server restarts.
    layout: Vec<keyboard::KeyCap>,
}

impl MacInput {
    /// Must run on the main thread.
    pub fn new() -> PortResult<Self> {
        Ok(Self { layout: keyboard::shortcut_layout()? })
    }
}

impl Input for MacInput {
    fn activate(&self, pid: i32, window_id: u32) -> PortResult<()> {
        // Connects this process to the window server (see screen.rs).
        CGMainDisplayID();
        let app = ui_tree::application(pid)?;
        let window = ui_tree::find_window(&app, pid, window_id)?;
        if in_place(&app, &window, window_id).is_some() {
            return Ok(());
        }
        // Setting AXFrontmost works from a background process without a run
        // loop, unlike NSRunningApplication activation since macOS 14.
        ui_tree::set_attribute(&app, &CFString::from_static_str("AXFrontmost"), CFBoolean::new(true))
            .map_err(|status| activation_error(status, pid))?;
        // Some windows do not support raising; the polling below decides.
        let _ = ui_tree::perform_action(&window, &CFString::from_static_str("AXRaise"));
        let deadline = Instant::now() + ACTIVATE_TIMEOUT;
        let mut last = None;
        while Instant::now() < deadline {
            sleep(ACTIVATE_POLL);
            let now = in_place(&app, &window, window_id);
            if now.is_some() && now == last {
                return Ok(());
            }
            last = now;
        }
        Err(format!(
            "window {window_id} did not come to the front at its full size within {} s. Its app may \
             be showing a dialog, the window may be on another Space or minimised, or the user may \
             be switching apps; call observe to see the current state, then try again",
            ACTIVATE_TIMEOUT.as_secs()
        ))
    }

    fn click(&self, window_id: u32, point: Point, button: Button, count: u8) -> PortResult<bool> {
        let (down, up, which) = match button {
            Button::Left => (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp, CGMouseButton::Left),
            Button::Right => (CGEventType::RightMouseDown, CGEventType::RightMouseUp, CGMouseButton::Right),
        };
        let target = CGPoint::new(point.x, point.y);
        // Every event exists before the first is posted, so a failure cannot
        // leave a button held down or the pointer away from where it was.
        let moves = Moves::new(target)?;
        let presses = (1..=count)
            .map(|click| Ok((mouse(down, target, which, click)?, mouse(up, target, which, click)?)))
            .collect::<PortResult<Vec<_>>>()?;
        if !topmost(window_id, target) {
            return Ok(false);
        }
        Ok(moves.around(|| {
            for (down, up) in &presses {
                // Moving the pointer can reveal the Dock, a menu bar or a
                // popover, and a banner can arrive at any time.
                if !topmost(window_id, target) {
                    return false;
                }
                post(down);
                post(up);
            }
            true
        }))
    }

    fn scroll(&self, window_id: u32, point: Point, dx: i32, dy: i32) -> PortResult<bool> {
        let target = CGPoint::new(point.x, point.y);
        let moves = Moves::new(target)?;
        let (vertical, horizontal) = wheel_deltas(dx, dy);
        let wheel = CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Line, 2, vertical, horizontal, 0)
            .ok_or(EVENT_FAILED)?;
        CGEvent::set_location(Some(&wheel), target);
        CGEvent::set_flags(Some(&wheel), CGEventFlags::empty());
        if !topmost(window_id, target) {
            return Ok(false);
        }
        // Scroll events go to the window under the pointer, hence the moves.
        Ok(moves.around(|| {
            let on_top = topmost(window_id, target);
            if on_top {
                post(&wheel);
            }
            on_top
        }))
    }

    fn type_text(&self, pid: i32, text: &str) -> PortResult<()> {
        for c in text.chars() {
            if c == '\n' {
                post_key(pid, keyboard::RETURN_KEY, CGEventFlags::empty(), None)?;
            } else {
                // The character travels as text, so the key code does not
                // matter and the user's keyboard layout cannot change it.
                post_key(pid, 0, CGEventFlags::empty(), Some(c))?;
            }
            sleep(KEY_PACE);
        }
        Ok(())
    }

    fn press_key(&self, pid: i32, combo: KeyCombo) -> PortResult<()> {
        let press = keyboard::press(combo, &self.layout)?;
        let mut flags = modifier_flags(combo.modifiers);
        flags.set(CGEventFlags::MaskShift, press.shift);
        post_key(pid, press.code, flags, press.text)
    }
}

/// The pointer's moves to a target and back to where it is now.
struct Moves {
    there: CFRetained<CGEvent>,
    back: CFRetained<CGEvent>,
}

impl Moves {
    fn new(target: CGPoint) -> PortResult<Self> {
        let now = CGEvent::new(None).ok_or(EVENT_FAILED)?;
        let original = CGEvent::location(Some(&now));
        Ok(Self {
            there: mouse(CGEventType::MouseMoved, target, CGMouseButton::Left, 0)?,
            back: mouse(CGEventType::MouseMoved, original, CGMouseButton::Left, 0)?,
        })
    }

    /// Runs `act` with the pointer at the target, then always puts it back.
    fn around(&self, act: impl FnOnce() -> bool) -> bool {
        post(&self.there);
        sleep(HOVER_SETTLE);
        let done = act();
        post(&self.back);
        done
    }
}

/// True only if a click at `point` would reach `window` now. Anything
/// uncertain (failed hit test, no window, a point off every display, a
/// window the server does not list as on screen) counts as covered.
fn topmost(window: u32, point: CGPoint) -> bool {
    CGMainDisplayID();
    let mut screen_point = point;
    let mut window_point = CGPoint::new(0.0, 0.0);
    let (mut hit, mut owner) = (0, 0);
    // The window server skips windows that let clicks through, such as
    // the invisible full-screen windows of screencaptureui, and counts
    // menus, the Dock, banners and Stage Manager's overlays like any other.
    // SAFETY: every pointer is valid for the duration of the call.
    let status = unsafe {
        CGSFindWindowAndOwner(
            CGSMainConnectionID(),
            0,
            1,
            0,
            &mut screen_point,
            &mut window_point,
            &mut hit,
            &mut owner,
        )
    };
    status == 0 && hit != 0 && hit == window && on_screen(window)
}

fn on_screen(window: u32) -> bool {
    let infos = window_infos(CGWindowListOption::OptionIncludingWindow, window);
    // SAFETY: a CoreGraphics constant, valid for the life of the process.
    let key = unsafe { kCGWindowIsOnscreen };
    infos.first().and_then(|info| info.get(key)).and_then(|v| v.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool))
        == Some(true)
}

/// The window's on-screen bounds, provided its app is frontmost, it is the
/// app's main or key window, and it is drawn at its real frame: not as a
/// Stage Manager thumbnail, and not part way through the animation out of one.
fn in_place(app: &AXUIElement, window: &AXUIElement, window_id: u32) -> Option<Rect> {
    let frontmost = ui_tree::flag(app, "AXFrontmost") == Ok(Some(true));
    // A window without the attribute cannot be main; it is judged by its frame.
    // Alerts and panels cannot become main either (AXMain is false, seen live
    // on an NSAlert) but have focus when they are in front.
    let main = matches!(ui_tree::flag(window, "AXMain"), Ok(Some(true) | None))
        || ui_tree::flag(window, "AXFocused") == Ok(Some(true));
    if !frontmost || !main {
        return None;
    }
    let real = ui_tree::frame(window).ok()??;
    let drawn = window_bounds(window_id)?;
    let same = |a: f64, b: f64| (a - b).abs() < 1.0;
    let settled = same(real.x, drawn.x)
        && same(real.y, drawn.y)
        && same(real.width, drawn.width)
        && same(real.height, drawn.height);
    settled.then_some(drawn)
}

fn activation_error(status: AXError, pid: i32) -> String {
    match status {
        AXError::APIDisabled | AXError::InvalidUIElement => ui_tree::ax_error(status, pid),
        _ => format!(
            "the app (pid {pid}) refused to come to the front (AXError {}); call list_windows to \
             check that it is still running",
            status.0
        ),
    }
}

/// Window descriptions from the window server, front to back.
fn window_infos(option: CGWindowListOption, window: u32) -> Vec<CFRetained<CFDictionary<CFString, CFType>>> {
    let Some(list) = CGWindowListCopyWindowInfo(option, window) else { return Vec::new() };
    // SAFETY: the window list is an array of dictionaries with string keys.
    let list = unsafe { list.cast_unchecked::<CFDictionary>() };
    list.iter().map(|info| unsafe { CFRetained::cast_unchecked(info) }).collect()
}

/// Where the window server draws the window now, in screen points.
fn window_bounds(window: u32) -> Option<Rect> {
    let info = window_infos(CGWindowListOption::OptionIncludingWindow, window).into_iter().next()?;
    // SAFETY: a CoreGraphics constant, valid for the life of the process.
    let bounds = info.get(unsafe { kCGWindowBounds })?;
    let bounds = bounds.downcast_ref::<CFDictionary>()?;
    let mut rect = CGRect::default();
    // SAFETY: `bounds` is the rect dictionary CoreGraphics made; `rect` is a valid out-pointer.
    unsafe { CGRectMakeWithDictionaryRepresentation(Some(bounds), &mut rect) }.then_some(Rect {
        x: rect.origin.x,
        y: rect.origin.y,
        width: rect.size.width,
        height: rect.size.height,
    })
}

/// `click` is the click state of a press (1, or 2 for a double click); 0 for a move.
fn mouse(kind: CGEventType, at: CGPoint, button: CGMouseButton, click: u8) -> PortResult<CFRetained<CGEvent>> {
    let event = CGEvent::new_mouse_event(None, kind, at, button).ok_or(EVENT_FAILED)?;
    if click > 0 {
        CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventClickState, click.into());
    }
    // Keys the user happens to hold must not turn a click into a cmd-click.
    CGEvent::set_flags(Some(&event), CGEventFlags::empty());
    Ok(event)
}

fn post(event: &CGEvent) {
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(event));
    sleep(POINTER_PAUSE);
}

/// Presses and releases a key in process `pid` only. Posting to the process
/// skips the system's event routing, so system-wide shortcuts (Spotlight,
/// the app switcher, screenshots) never see the key.
fn post_key(pid: i32, code: CGKeyCode, flags: CGEventFlags, text: Option<char>) -> PortResult<()> {
    let key = |down| -> PortResult<CFRetained<CGEvent>> {
        let event = CGEvent::new_keyboard_event(None, code, down).ok_or(EVENT_FAILED)?;
        CGEvent::set_flags(Some(&event), flags);
        if let Some(c) = text {
            let mut buffer = [0; 2];
            let units = c.encode_utf16(&mut buffer);
            // SAFETY: `units` is valid for `units.len()` UTF-16 code units.
            unsafe { CGEvent::keyboard_set_unicode_string(Some(&event), units.len() as _, units.as_ptr()) };
        }
        Ok(event)
    };
    // Both exist before either is posted, so a key is never left held down.
    let (down, up) = (key(true)?, key(false)?);
    CGEvent::post_to_pid(pid, Some(&down));
    CGEvent::post_to_pid(pid, Some(&up));
    Ok(())
}

fn modifier_flags(modifiers: Modifiers) -> CGEventFlags {
    let mut flags = CGEventFlags::empty();
    flags.set(CGEventFlags::MaskCommand, modifiers.cmd);
    flags.set(CGEventFlags::MaskControl, modifiers.ctrl);
    flags.set(CGEventFlags::MaskAlternate, modifiers.alt);
    flags.set(CGEventFlags::MaskShift, modifiers.shift);
    flags
}

/// CoreGraphics wheel deltas (vertical, horizontal) for "scroll content by
/// lines". A positive wheel delta turns the wheel the way that reveals what is
/// above or to the left, the opposite of the port's positive `dy` and `dx`.
fn wheel_deltas(dx: i32, dy: i32) -> (i32, i32) {
    (dy.saturating_neg(), dx.saturating_neg())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_scroll_reveals_content_below_and_to_the_right() {
        assert_eq!(wheel_deltas(0, 3), (-3, 0));
        assert_eq!(wheel_deltas(-2, -1), (1, 2));
    }
}
