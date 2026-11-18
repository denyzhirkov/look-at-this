//! An app's menu bar through accessibility: menus are read through their
//! children and items pressed with AXPress, so no menu opens on screen.

use std::time::{Duration, Instant};

use objc2_application_services::{AXError, AXUIElement};
use objc2_core_foundation::{CFNumber, CFRetained, CFString};

use super::ui_tree::{act, application, ax_error, children, copy_attribute, flag, frame, menu_child, text};
use crate::ports::{MenuItem, MenuKind, MenuStep, Modifiers, PortResult};

/// Budget for reading one menu's items; each request already has its own
/// timeout, this bounds an app that answers each one slowly.
const READ_DEADLINE: Duration = Duration::from_secs(4);

/// AppKit's identifiers for items of the system Services menu: each service,
/// and "Services Settings…". The menu's own title is in the user's language,
/// and it also holds separators and section headers with no identifier.
const SERVICE: &str = "_processRequest:";
const SERVICES_SETTINGS: &str = "_configureServicesMenu:";

/// Bits of `AXMenuItemCmdModifiers` (kAXMenuItemModifier…). Command is
/// implied unless NO_COMMAND is set. Other bits occur too, such as 16 for
/// the Globe key of "Emoji & Symbols" (seen live).
const SHIFT: i64 = 1;
const OPTION: i64 = 2;
const CONTROL: i64 = 4;
const NO_COMMAND: i64 = 8;
const KNOWN: i64 = SHIFT | OPTION | CONTROL | NO_COMMAND;

/// Where a path leads.
struct Reached {
    /// The menu holding the items below: the menu bar for the empty path,
    /// else the last item's submenu, if it has one.
    menu: Option<CFRetained<AXUIElement>>,
    /// The last item of the path.
    item: Option<CFRetained<AXUIElement>>,
}

pub(super) fn items(pid: i32, path: &[MenuStep]) -> PortResult<Option<Vec<MenuItem>>> {
    let Some(reached) = walk(pid, path)? else { return Ok(None) };
    let Some(menu) = reached.menu else { return Ok(Some(Vec::new())) };
    let deadline = Instant::now() + READ_DEADLINE;
    let at_bar = path.is_empty();
    let mut items = Vec::new();
    for (index, item) in children(&menu).map_err(|status| menu_error(status, pid))?.iter().enumerate() {
        if Instant::now() >= deadline {
            return Err(format!(
                "the app (pid {pid}) answers too slowly, so its menu could not be read in time; try again later"
            ));
        }
        let apple = at_bar && index == 0;
        items.push(read(item, apple, at_bar).map_err(|status| menu_error(status, pid))?);
    }
    Ok(Some(items))
}

pub(super) fn press(pid: i32, path: &[MenuStep]) -> PortResult<bool> {
    let Some(Reached { item: Some(item), .. }) = walk(pid, path)? else { return Ok(false) };
    // Pressing a closed menu's item performs it without showing the menu.
    act(&item, &CFString::from_static_str("AXPress"))?;
    Ok(true)
}

/// Follows `path` from the app's menu bar, checking each item's title and
/// kind just before going on, since menus change; `None` if an item is not
/// at its index with its title, or is not of kind `App`.
fn walk(pid: i32, path: &[MenuStep]) -> PortResult<Option<Reached>> {
    let fail = |status| menu_error(status, pid);
    let app = application(pid)?;
    let bar = copy_attribute(&app, &CFString::from_static_str("AXMenuBar"))
        .map_err(fail)?
        .and_then(|bar| bar.downcast::<AXUIElement>().ok())
        .ok_or("the app has no menu bar that accessibility can reach")?;
    let mut reached = Reached { menu: Some(bar), item: None };
    for (depth, step) in path.iter().enumerate() {
        let Some(menu) = reached.menu else { return Ok(None) };
        let Some(item) = children(&menu).map_err(fail)?.into_iter().nth(step.index) else { return Ok(None) };
        if text(&item, "AXTitle").map_err(fail)? != step.title {
            return Ok(None);
        }
        let submenu = menu_child(&item).map_err(fail)?;
        let apple = depth == 0 && step.index == 0;
        if kind(&item, apple, submenu.as_deref()).map_err(fail)? != MenuKind::App {
            return Ok(None);
        }
        reached = Reached { menu: submenu, item: Some(item) };
    }
    Ok(Some(reached))
}

/// One item of a menu; `apple` says it is the Apple menu. Frames are read
/// for menu-bar titles only: the items of a closed menu report none.
fn read(item: &AXUIElement, apple: bool, at_bar: bool) -> Result<MenuItem, AXError> {
    let submenu = menu_child(item)?;
    Ok(MenuItem {
        title: text(item, "AXTitle")?,
        // An item that does not say it is enabled is not taken to be.
        enabled: flag(item, "AXEnabled")?.unwrap_or(false),
        checked: !text(item, "AXMenuItemMarkChar")?.is_empty(),
        has_submenu: submenu.is_some(),
        shortcut: shortcut(item)?,
        kind: kind(item, apple, submenu.as_deref())?,
        frame: if at_bar { frame(item)? } else { None },
    })
}

/// What an item is: `apple` says it is the Apple menu; otherwise it is told
/// by its own identifier and those of every item of its submenu.
fn kind(item: &AXUIElement, apple: bool, submenu: Option<&AXUIElement>) -> Result<MenuKind, AXError> {
    if apple {
        return Ok(MenuKind::Apple);
    }
    if is_service(item)? {
        return Ok(MenuKind::ServicesItem);
    }
    if let Some(submenu) = submenu {
        for entry in children(submenu)? {
            if is_service(&entry)? {
                return Ok(MenuKind::Services);
            }
        }
    }
    Ok(MenuKind::App)
}

fn is_service(item: &AXUIElement) -> Result<bool, AXError> {
    let identifier = text(item, "AXIdentifier")?;
    Ok(identifier == SERVICE || identifier == SERVICES_SETTINGS)
}

/// The item's shortcut when it is a printable key with modifiers press_key
/// knows. Other keys are reported as symbols such as ⎋, which cannot be
/// named for press_key with certainty.
fn shortcut(item: &AXUIElement) -> Result<Option<(Modifiers, char)>, AXError> {
    let key = text(item, "AXMenuItemCmdChar")?;
    let mut chars = key.chars();
    let (Some(key), None) = (chars.next(), chars.next()) else { return Ok(None) };
    if !key.is_ascii_graphic() {
        return Ok(None);
    }
    let bits = copy_attribute(item, &CFString::from_static_str("AXMenuItemCmdModifiers"))?
        .and_then(|bits| bits.downcast_ref::<CFNumber>()?.as_i64())
        .unwrap_or(0);
    if bits & !KNOWN != 0 {
        return Ok(None);
    }
    let modifiers = Modifiers {
        cmd: bits & NO_COMMAND == 0,
        ctrl: bits & CONTROL != 0,
        alt: bits & OPTION != 0,
        shift: bits & SHIFT != 0,
    };
    // Letters are reported in upper case whether or not Shift is part of it.
    Ok(Some((modifiers, key.to_ascii_lowercase())))
}

fn menu_error(status: AXError, pid: i32) -> String {
    match status {
        AXError::InvalidUIElement => "the menu changed while it was being read; call menu again".to_string(),
        AXError::CannotComplete => format!(
            "the app (pid {pid}) is not answering accessibility requests, so its menus cannot be read \
             now; call list_windows to check it is still running"
        ),
        AXError::APIDisabled => ax_error(status, pid),
        status => format!("reading the app's menus failed (AXError {})", status.0),
    }
}
