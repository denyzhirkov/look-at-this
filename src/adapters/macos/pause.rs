//! The emergency stop: a global hotkey that pauses all agent input.
//!
//! Every server process registers ctrl+option+cmd+escape, and macOS delivers
//! each press to all of them. The pause lives in one small state file that
//! every process reads before each action: `paused <unix-millis>` or
//! `running <unix-millis>`, the time of the last change. On a press each
//! process decides on its own: if the last change answered this press
//! already, it does nothing; otherwise it writes the opposite state.
//! Processes racing on one press compute the same new state, so it does not
//! matter whose write lands last, and none depends on another being healthy.
//! The state outlives the servers on purpose, since only the user lifts an
//! emergency stop: a server started later starts paused.

use std::cell::{Cell, OnceCell};
use std::ffi::{OsString, c_void};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, io, process, ptr};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchTime};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAnimatablePropertyContainer, NSApplication, NSColor, NSEventType, NSFont, NSTextField, NSWindow,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGDisplayBounds, CGMainDisplayID};
use objc2_foundation::NSString;

use super::overlay::{animate, click_through_window};
use crate::ports::Pause;

const PAUSED: &str = "Agent input paused — ⌃⌥⌘⎋ to resume";
const RESUMED: &str = "Agent input resumed";
/// A change this soon before a press answered it: every process gets the
/// press within a millisecond or so, while a person cannot press twice this fast.
const DEBOUNCE_MS: u64 = 400;

const FONT_SIZE: f64 = 22.0;
const PADDING: (f64, f64) = (28.0, 16.0);
const CORNER_RADIUS: f64 = 14.0;
const APPEAR_SECS: f64 = 0.15;
const FADE_SECS: f64 = 0.4;
/// How long a notice stays fully visible.
const VISIBLE: Duration = Duration::from_millis(2500);

// Carbon Event Manager, which objc2 does not cover.
type OsStatus = i32;
type EventHandler = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> OsStatus;

#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
struct EventHotKeyId {
    signature: u32,
    id: u32,
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> *mut c_void;
    fn InstallEventHandler(
        target: *mut c_void,
        handler: EventHandler,
        type_count: usize,
        types: *const EventTypeSpec,
        user_data: *mut c_void,
        handler_ref: *mut *mut c_void,
    ) -> OsStatus;
    fn RegisterEventHotKey(
        key_code: u32,
        modifiers: u32,
        id: EventHotKeyId,
        target: *mut c_void,
        options: u32,
        hotkey_ref: *mut *mut c_void,
    ) -> OsStatus;
    /// Seconds since startup, the clock of `NSEvent::timestamp`.
    fn GetCurrentEventTime() -> f64;
}

const K_EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
const K_EVENT_HOT_KEY_PRESSED: u32 = 5;
const KEY_ESCAPE: u32 = 53;
/// cmdKey | optionKey | controlKey.
const MODIFIERS: u32 = (1 << 8) | (1 << 11) | (1 << 12);

pub struct MacPause {
    state_file: PathBuf,
}

impl MacPause {
    /// Registers the hotkey that toggles the pause kept in `state_file`. Call
    /// on the main thread once `NSApplication` is set up, before its event loop.
    pub fn install(mtm: MainThreadMarker, state_file: PathBuf) -> Result<Self, String> {
        let dir = state_file.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        // A press must be able to write the state, so find out now rather than then.
        let probe = temp_path(&state_file);
        fs::write(&probe, "")
            .and_then(|()| fs::remove_file(&probe))
            .map_err(|e| format!("cannot write in {}: {e}", dir.display()))?;
        STATE_FILE
            .with(|cell| cell.set(state_file.clone()))
            .map_err(|_| "the emergency stop is already installed")?;
        register_hotkey(mtm)?;
        let pause = Self { state_file };
        if pause.is_paused() {
            // Once the event loop runs, so the user knows why the agent cannot act.
            DispatchQueue::main().exec_async(|| show_notice(PAUSED));
        }
        Ok(pause)
    }
}

impl Pause for MacPause {
    fn is_paused(&self) -> bool {
        read_state(&self.state_file).0 == State::Paused
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Running,
    Paused,
}

impl State {
    fn flipped(self) -> Self {
        match self {
            Self::Running => Self::Paused,
            Self::Paused => Self::Running,
        }
    }
}

/// The state and the Unix milliseconds of its last change. A missing file
/// is running; one that cannot be read or parsed is paused, with no time:
/// an emergency stop fails closed.
fn read_state(path: &Path) -> (State, Option<u64>) {
    match fs::read_to_string(path) {
        Ok(text) => parse(&text).map_or((State::Paused, None), |(state, at)| (state, Some(at))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (State::Running, None),
        Err(_) => (State::Paused, None),
    }
}

fn parse(text: &str) -> Option<(State, u64)> {
    let (word, millis) = text.trim().split_once(' ')?;
    let state = match word {
        "paused" => State::Paused,
        "running" => State::Running,
        _ => return None,
    };
    Some((state, millis.parse().ok()?))
}

/// What a press at `pressed_at` should write, or `None` if the last change
/// already answered it: made by another process for the same press, or after
/// the press, which a process that was stopped or busy may handle late. A
/// change later than `now` means the clock went back, so it counts as old.
fn answer(state: State, changed_at: Option<u64>, pressed_at: u64, now: u64) -> Option<State> {
    match changed_at {
        Some(at) if at <= now && at + DEBOUNCE_MS > pressed_at => None,
        _ => Some(state.flipped()),
    }
}

/// Replaces the state file in one step, so a reader never sees half of it.
fn write_state(path: &Path, state: State, now: u64) -> io::Result<()> {
    let word = match state {
        State::Running => "running",
        State::Paused => "paused",
    };
    let temp = temp_path(path);
    fs::write(&temp, format!("{word} {now}\n"))?;
    fs::rename(&temp, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

/// Beside `path`, so the rename stays on one file system; one per process.
fn temp_path(path: &Path) -> PathBuf {
    let mut temp = OsString::from(path.as_os_str());
    temp.push(format!(".{}.tmp", process::id()));
    PathBuf::from(temp)
}

fn unix_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

thread_local! {
    /// Set on the main thread by `install`, where Carbon calls `on_hotkey`.
    static STATE_FILE: OnceCell<PathBuf> = const { OnceCell::new() };
    /// Lives on the main thread: `with_notice` reaches it only there.
    static NOTICE: OnceCell<Option<Notice>> = const { OnceCell::new() };
}

fn register_hotkey(_mtm: MainThreadMarker) -> Result<(), String> {
    let pressed = EventTypeSpec { event_class: K_EVENT_CLASS_KEYBOARD, event_kind: K_EVENT_HOT_KEY_PRESSED };
    // SAFETY: on the main thread, as Carbon events require. `pressed` is read
    // during the call only; the handler stays installed for the life of the process.
    let target = unsafe { GetApplicationEventTarget() };
    let status = unsafe { InstallEventHandler(target, on_hotkey, 1, &pressed, ptr::null_mut(), ptr::null_mut()) };
    if status != 0 {
        return Err(format!("cannot install the emergency stop handler (OSStatus {status})"));
    }
    let id = EventHotKeyId { signature: u32::from_be_bytes(*b"LATH"), id: 1 };
    let mut hotkey = ptr::null_mut();
    // SAFETY: as above; `hotkey` outlives the call. The hotkey is never unregistered.
    let status = unsafe { RegisterEventHotKey(KEY_ESCAPE, MODIFIERS, id, target, 0, &mut hotkey) };
    if status != 0 {
        return Err(format!(
            "cannot register the emergency stop hotkey ctrl+option+cmd+escape (OSStatus {status})"
        ));
    }
    Ok(())
}

/// Carbon calls this on the main thread for every press of the hotkey.
extern "C" fn on_hotkey(_call: *mut c_void, _event: *mut c_void, _data: *mut c_void) -> OsStatus {
    let age_ms = (press_age() * 1000.0) as u64;
    STATE_FILE.with(|path| {
        if let Some(path) = path.get() {
            press(path, age_ms);
        }
    });
    0
}

/// Seconds since the key was pressed: more than a moment if this process was
/// stopped or busy. AppKit's event keeps the time of the press; the Carbon
/// event's own time is when this process got around to it.
fn press_age() -> f64 {
    let Some(mtm) = MainThreadMarker::new() else { return 0.0 };
    let Some(event) = NSApplication::sharedApplication(mtm).currentEvent() else { return 0.0 };
    if event.r#type() != NSEventType::SystemDefined {
        return 0.0;
    }
    // SAFETY: a plain Carbon clock read.
    (unsafe { GetCurrentEventTime() } - event.timestamp()).max(0.0)
}

/// Answers a press that happened `age_ms` ago.
fn press(path: &Path, age_ms: u64) {
    let (state, changed_at) = read_state(path);
    // The clock is read after the state: every server handles the same press
    // at once, and a change another one has just written must not look as if
    // it came from the future, or `answer` would flip it straight back.
    let now = unix_millis();
    let shown = match answer(state, changed_at, now.saturating_sub(age_ms), now) {
        // Already answered: still confirm the state, so every press gets a notice.
        None => Ok(state),
        Some(new) => write_state(path, new, now).map(|()| new),
    };
    match shown {
        Ok(State::Paused) => show_notice(PAUSED),
        Ok(State::Running) => show_notice(RESUMED),
        Err(error) => show_notice(&format!("Emergency stop failed: {error}")),
    }
}

fn show_notice(text: &str) {
    with_notice(|notice| notice.show(text));
}

/// Runs `act` on the notice, creating it on first use. Does nothing off the
/// main thread or if AppKit could not make the window.
fn with_notice(act: impl FnOnce(&Notice)) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    NOTICE.with(|notice| {
        if let Some(notice) = notice.get_or_init(|| Notice::new(mtm)) {
            act(notice);
        }
    });
}

/// A short message in the middle of the main display, in the same
/// click-through kind of window as the agent's cursor.
struct Notice {
    window: Retained<NSWindow>,
    label: Retained<NSTextField>,
    /// Counts `show` calls, so a fade scheduled by an earlier one can tell
    /// that another message has been shown since.
    shown: Cell<u64>,
}

impl Notice {
    fn new(mtm: MainThreadMarker) -> Option<Self> {
        let window = click_through_window(mtm, CGSize::new(1.0, 1.0));
        let label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        label.setFont(Some(&NSFont::boldSystemFontOfSize(FONT_SIZE)));
        label.setTextColor(Some(&NSColor::whiteColor()));
        let view = window.contentView()?;
        view.setWantsLayer(true);
        let layer = view.layer()?;
        layer.setBackgroundColor(Some(&CGColor::new_srgb(0.1, 0.1, 0.1, 0.9)));
        layer.setCornerRadius(CORNER_RADIUS);
        view.addSubview(&label);
        Some(Self { window, label, shown: Cell::new(0) })
    }

    fn show(&self, text: &str) {
        let shown = self.shown.get() + 1;
        self.shown.set(shown);
        self.label.setStringValue(&NSString::from_str(text));
        self.label.sizeToFit();
        self.label.setFrameOrigin(CGPoint::new(PADDING.0, PADDING.1));
        let text = self.label.frame().size;
        let size = CGSize::new(text.width + 2.0 * PADDING.0, text.height + 2.0 * PADDING.1);
        let display = CGDisplayBounds(CGMainDisplayID()).size;
        let origin = CGPoint::new((display.width - size.width) / 2.0, (display.height - size.height) / 2.0);
        let window = &self.window;
        window.setFrame_display(CGRect::new(origin, size), true);
        // Through the animator, which also cancels a fade in progress.
        animate(APPEAR_SECS, || window.animator().setAlphaValue(1.0), None);
        // Does not activate this process, unlike orderFront.
        window.orderFrontRegardless();
        if let Ok(when) = DispatchTime::try_from(VISIBLE) {
            let _ = DispatchQueue::main().after(when, move || with_notice(|notice| notice.fade(shown)));
        }
    }

    /// Fades the notice out, unless another was shown after call `shown`.
    fn fade(&self, shown: u64) {
        if self.shown.get() != shown {
            return;
        }
        let window = &self.window;
        let done = RcBlock::new(move || {
            with_notice(|notice| {
                if notice.shown.get() == shown {
                    notice.window.orderOut(None);
                }
            })
        });
        animate(FADE_SECS, || window.animator().setAlphaValue(0.0), Some(&done));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_press_flips_the_state_unless_a_recent_change_answered_it() {
        let (now, pressed) = (1_000_000, 999_990);
        // Another process answered this press a moment ago, or a late
        // handler finds a change made after the press.
        assert_eq!(answer(State::Paused, Some(pressed - 5), pressed, now), None);
        assert_eq!(answer(State::Paused, Some(pressed + 8), pressed, now), None);
        // A second press within the debounce window counts as the same press.
        assert_eq!(answer(State::Running, Some(pressed - DEBOUNCE_MS + 1), pressed, now), None);
        // An old change, no change yet, or one from a clock that went back.
        assert_eq!(answer(State::Paused, Some(pressed - DEBOUNCE_MS), pressed, now), Some(State::Running));
        assert_eq!(answer(State::Running, None, pressed, now), Some(State::Paused));
        assert_eq!(answer(State::Running, Some(now + 1), pressed, now), Some(State::Paused));
    }

    #[test]
    fn the_state_file_round_trips_and_anything_unreadable_is_paused() {
        let dir = std::env::temp_dir().join(format!("look-at-this-pause-{}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("paused");
        assert_eq!(read_state(&path), (State::Running, None));
        write_state(&path, State::Paused, 42).unwrap();
        assert_eq!(read_state(&path), (State::Paused, Some(42)));
        write_state(&path, State::Running, 43).unwrap();
        assert_eq!(read_state(&path), (State::Running, Some(43)));
        // The file of the earlier design, or anything else.
        fs::write(&path, "look-at-this: agent input is paused.\n").unwrap();
        assert_eq!(read_state(&path), (State::Paused, None));
        // Unreadable, so paused: a press resumes, but here it cannot write.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert_eq!(read_state(&path), (State::Paused, None));
        assert!(write_state(&path, State::Running, 44).is_err());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "no temp file is left behind");
        fs::remove_dir_all(&dir).unwrap();
    }
}
