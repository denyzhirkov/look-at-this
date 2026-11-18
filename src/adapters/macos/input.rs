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
use crate::ports::{Button, DragCancel, DragOutcome, DragUndo, Input, Key, KeyCombo, Modifiers, Point, PortResult, Rect};

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
/// A drag moves the pointer in this many steps, each a `post`, a
/// `DRAG_STEP_PAUSE` and its checks apart, since apps ignore a press that
/// jumps straight to its end point.
const DRAG_STEPS: u32 = 16;
const DRAG_STEP_PAUSE: Duration = Duration::from_millis(15);
/// Between the press and the first step. A text view drags a selection
/// pressed for longer than about 0.1 s and starts a new selection otherwise
/// (TextEdit live: a 75 ms hold selected, 115 ms dragged).
const DRAG_HOLD: Duration = Duration::from_millis(200);
/// After Escape is sent to cancel a drag-and-drop, before the release.
const ESCAPE_SETTLE: Duration = Duration::from_millis(100);
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
    /// Public; declared here because the crate binding needs a feature this
    /// crate does not otherwise use.
    fn CGGetDisplaysWithPoint(point: CGPoint, max_displays: u32, displays: *mut u32, count: *mut u32) -> i32;
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
        let window = match ui_tree::lookup_window(&app, pid, window_id)? {
            Some(window) => window,
            // On screen but not exposed by its app: the usual explanation.
            None if on_screen(window_id) => ui_tree::find_window(&app, pid, window_id)?,
            // The window server lists it, accessibility does not: nothing
            // here can raise it, and observe would only send the agent back.
            None => return Err(format!(
                "window {window_id} cannot be brought to the front: it is on another Space, where \
                 accessibility does not reach it, or it has just closed. Call list_windows; if it \
                 is still listed, ask the user to switch to its Space or move it to this one"
            )),
        };
        if in_place(&app, &window, window_id).is_some() {
            return Ok(());
        }
        // Raising does not bring a minimised window back from the Dock.
        let minimised = ui_tree::flag(&window, "AXMinimized") == Ok(Some(true));
        if minimised {
            ui_tree::set_attribute(&window, &CFString::from_static_str("AXMinimized"), CFBoolean::new(false))
                .map_err(|status| activation_error(status, pid))?;
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
            if minimised && now.is_none() {
                // A window still coming back from the Dock ignores the first
                // raise, and another window of the app stays in front. Only
                // then: raising again would otherwise fight a user who has
                // just pressed the emergency stop and picked another window.
                let _ = ui_tree::perform_action(&window, &CFString::from_static_str("AXRaise"));
            }
            last = now;
        }
        Err(format!(
            "window {window_id} did not come to the front at its full size within {} s. Its app may \
             be showing a dialog, the window may be on another Space, or the user may be switching \
             apps; call list_windows to see the current state, then try again",
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

    fn drag(
        &self,
        pid: i32,
        window_id: u32,
        from: Point,
        to: Point,
        stop: &dyn Fn() -> bool,
    ) -> PortResult<DragOutcome> {
        let (start, end) = (CGPoint::new(from.x, from.y), CGPoint::new(to.x, to.y));
        // Every event exists before the first is posted, as for click.
        let moves = Moves::new(start)?;
        let steps = (0..=DRAG_STEPS)
            .map(|i| {
                let t = f64::from(i) / f64::from(DRAG_STEPS);
                let at = CGPoint::new(start.x + (end.x - start.x) * t, start.y + (end.y - start.y) * t);
                Ok(Step {
                    at,
                    drag: mouse(CGEventType::LeftMouseDragged, at, CGMouseButton::Left, 1)?,
                    up: mouse(CGEventType::LeftMouseUp, at, CGMouseButton::Left, 1)?,
                })
            })
            .collect::<PortResult<Vec<_>>>()?;
        let down = mouse(CGEventType::LeftMouseDown, start, CGMouseButton::Left, 1)?;
        let escape = keyboard::press(KeyCombo { modifiers: Modifiers::default(), key: Key::Escape }, &self.layout)?;
        let escape = key_events(escape.code, CGEventFlags::empty(), None)?;
        // Off every display the pointer stops at the edge, and the path
        // would end somewhere else than it says.
        if !steps.iter().all(|step| on_display(step.at)) {
            return Ok(DragOutcome::OffDisplay);
        }
        if !topmost(window_id, start) {
            return Ok(DragOutcome::NotOnTop);
        }
        Ok(moves.around(|| {
            if !topmost(window_id, start) {
                return DragOutcome::NotOnTop;
            }
            let Some(frame) = window_bounds(window_id) else { return DragOutcome::NotOnTop };
            post(&down);
            let mut held = Held { pid, window_id, steps: &steps, escape: &escape, taken: 0, released: false };
            sleep(DRAG_HOLD);
            for (i, step) in steps.iter().enumerate().skip(1) {
                if stop() {
                    return DragOutcome::Cancelled { why: DragCancel::Stopped, undo: held.cancel() };
                }
                post(&step.drag);
                held.taken = i;
                sleep(DRAG_STEP_PAUSE);
                // Positions are posted absolute, so a mouse the user moves
                // only makes the pointer jump: caught by where it is now.
                let at = step.at;
                if !pointer().is_some_and(|now| (now.x - at.x).abs() < 1.0 && (now.y - at.y).abs() < 1.0) {
                    return DragOutcome::Cancelled { why: DragCancel::PointerMoved, undo: held.cancel() };
                }
                // Whatever is on top gets the drag's events, and with
                // drag-and-drop it can read what is dragged.
                if !topmost(window_id, at) {
                    return DragOutcome::Cancelled { why: DragCancel::Covered, undo: held.cancel() };
                }
            }
            // What the drag shows (a selection, a moved thumb) is drawn
            // before the window is asked about again.
            sleep(HOVER_SETTLE);
            if !topmost(window_id, end) || window_bounds(window_id) != Some(frame) {
                return DragOutcome::Cancelled { why: DragCancel::Covered, undo: held.cancel() };
            }
            held.release_here();
            DragOutcome::Dropped
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
        let original = pointer().ok_or(EVENT_FAILED)?;
        Ok(Self {
            there: mouse(CGEventType::MouseMoved, target, CGMouseButton::Left, 0)?,
            back: mouse(CGEventType::MouseMoved, original, CGMouseButton::Left, 0)?,
        })
    }

    /// Runs `act` with the pointer at the target, then always puts it back.
    fn around<T>(&self, act: impl FnOnce() -> T) -> T {
        post(&self.there);
        sleep(HOVER_SETTLE);
        let done = act();
        post(&self.back);
        done
    }
}

/// One point of a drag's path, with the events that move there and release there.
struct Step {
    at: CGPoint,
    drag: CFRetained<CGEvent>,
    up: CFRetained<CGEvent>,
}

/// The left button, pressed for a drag. Each return from the drag releases
/// it, through `release_here` or `cancel`, and dropping it unreleased
/// cancels. A process that dies mid-drag releases nothing (see `Input::drag`).
struct Held<'a> {
    pid: i32,
    window_id: u32,
    /// The drag's path, the start first.
    steps: &'a [Step],
    escape: &'a [CFRetained<CGEvent>; 2],
    /// Index of the step the pointer is at.
    taken: usize,
    released: bool,
}

impl Held<'_> {
    fn release_here(&mut self) {
        self.released = true;
        post(&self.steps[self.taken].up);
    }

    /// Releases the button as close to the start as is safe: something
    /// dragged by drag-and-drop lands wherever the pointer is at the release,
    /// and is shown to whatever the pointer crosses on the way there, so
    /// only where the window is on top and only back over such points.
    fn cancel(&mut self) -> DragUndo {
        self.released = true;
        // Hit tests do not need the pointer there, so the point is found
        // before the pointer moves, and it never goes back over a covered one.
        if let Some(i) = retreat(self.taken, |i| topmost(self.window_id, self.steps[i].at)) {
            for step in self.steps[i..self.taken].iter().rev() {
                post(&step.drag);
            }
            self.taken = i;
            // Asked again: retracing takes time.
            if topmost(self.window_id, self.steps[i].at) {
                post(&self.steps[i].up);
                return if i == 0 { DragUndo::AtStart } else { DragUndo::OnPath };
            }
        }
        let [down, up] = self.escape;
        CGEvent::post_to_pid(self.pid, Some(down));
        CGEvent::post_to_pid(self.pid, Some(up));
        sleep(ESCAPE_SETTLE);
        post(&self.steps[self.taken].up);
        DragUndo::Escaped
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        if !self.released {
            self.cancel();
        }
    }
}

/// The step a cancelled drag goes back to for its release, with the pointer
/// at step `taken`: the one nearest the start that is reached over free
/// steps only, itself free (`free` is asked from `taken - 1` down and no
/// further than the first step that is not). `None` when the step right
/// behind the pointer is not free, or the start is not with the pointer
/// still on it. The step the pointer is on is otherwise not asked about:
/// leaving it shows nothing more to what covers it.
fn retreat(taken: usize, free: impl Fn(usize) -> bool) -> Option<usize> {
    (0..taken.max(1)).rev().take_while(|&i| free(i)).last()
}

/// Whether `point` is on one of the displays.
fn on_display(point: CGPoint) -> bool {
    let mut count = 0;
    // SAFETY: a null display list with a zero maximum only counts; `count` is a valid out-pointer.
    let status = unsafe { CGGetDisplaysWithPoint(point, 0, std::ptr::null_mut(), &mut count) };
    status == 0 && count > 0
}

/// Where the pointer is now.
fn pointer() -> Option<CGPoint> {
    CGEvent::new(None).map(|now| CGEvent::location(Some(&now)))
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
    window_infos(CGWindowListOption::OptionIncludingWindow, window).first().is_some_and(|info| shown(info))
}

/// Whether a window description from the window server says it is on screen.
pub(super) fn shown(info: &CFDictionary<CFString, CFType>) -> bool {
    // SAFETY: a CoreGraphics constant, valid for the life of the process.
    let key = unsafe { kCGWindowIsOnscreen };
    info.get(key).and_then(|v| v.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool)) == Some(true)
}

/// The window's on-screen bounds, provided its app is frontmost, it is the
/// app's main or key window, it is on screen, and it is drawn at its real
/// frame: not as a Stage Manager thumbnail, and not part way through the
/// animation out of one.
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
pub(super) fn window_infos(
    option: CGWindowListOption,
    window: u32,
) -> Vec<CFRetained<CFDictionary<CFString, CFType>>> {
    let Some(list) = CGWindowListCopyWindowInfo(option, window) else { return Vec::new() };
    // SAFETY: the window list is an array of dictionaries with string keys.
    let list = unsafe { list.cast_unchecked::<CFDictionary>() };
    list.iter().map(|info| unsafe { CFRetained::cast_unchecked(info) }).collect()
}

/// Where the window server draws the window now, in screen points; `None`
/// unless it is on screen. A window on another Space keeps its bounds, and
/// raising it there can make it main without switching to that Space.
fn window_bounds(window: u32) -> Option<Rect> {
    let info = window_infos(CGWindowListOption::OptionIncludingWindow, window).into_iter().next()?;
    if !shown(&info) {
        return None;
    }
    bounds(&info)
}

/// A window description's bounds, in screen points with the origin at the
/// top-left of the main display: the space of SCWindow.frame (equal for every
/// on-screen window compared live) and of accessibility frames, which
/// `in_place` compares them with.
pub(super) fn bounds(info: &CFDictionary<CFString, CFType>) -> Option<Rect> {
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
    // Both exist before either is posted, so a key is never left held down.
    let [down, up] = key_events(code, flags, text)?;
    CGEvent::post_to_pid(pid, Some(&down));
    CGEvent::post_to_pid(pid, Some(&up));
    Ok(())
}

/// The press and the release of a key.
fn key_events(code: CGKeyCode, flags: CGEventFlags, text: Option<char>) -> PortResult<[CFRetained<CGEvent>; 2]> {
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
    Ok([key(true)?, key(false)?])
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

    #[test]
    fn cancelled_drag_goes_back_only_over_free_steps() {
        let to = |taken: usize, covered: &[usize]| retreat(taken, |i| !covered.contains(&i));
        // The whole travelled path is free: back to the start.
        assert_eq!(to(10, &[]), Some(0));
        // Only the step the pointer is on is covered: it is left behind.
        assert_eq!(to(10, &[10]), Some(0));
        // Covered from step 4 on: never back through steps 9..4 to the free start.
        assert_eq!(to(10, &[4, 5, 6, 7, 8, 9, 10]), None);
        // The start is covered: the last free step before it.
        assert_eq!(to(10, &[0, 1, 2]), Some(3));
        // A covered stretch in the middle is not crossed.
        assert_eq!(to(10, &[5]), Some(6));
        // The step right behind the pointer is covered, wherever the pointer is.
        assert_eq!(to(10, &[9]), None);
        assert_eq!(to(1, &[0]), None);
        // The pointer has not left the start.
        assert_eq!(to(0, &[]), Some(0));
        assert_eq!(to(0, &[0]), None);
    }

    #[test]
    fn cancelled_drag_asks_no_further_than_the_first_covered_step() {
        let asked = std::cell::RefCell::new(Vec::new());
        let found = retreat(10, |i| {
            asked.borrow_mut().push(i);
            i > 7
        });
        assert_eq!(found, Some(8));
        assert_eq!(*asked.borrow(), [9, 8, 7]);
    }
}
