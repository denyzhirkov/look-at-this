//! Accessibility tree of a window via the AXUIElement API.

use std::ptr::{self, NonNull};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use objc2_application_services::{AXCopyMultipleAttributeOptions, AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFIndex, CFNull, CFNumber, CFRange, CFRetained, CFString, CFType, CGPoint, CGSize,
};

use crate::ports::{ElementHandle, Point, PortResult, Rect, UiAction, UiActions, UiNode, UiTree, UiTreeSnapshot};

/// Seconds one element may take to answer, so a hung app cannot block us.
const MESSAGING_TIMEOUT_SECS: f32 = 1.5;
/// Budget for one whole tree; what was read by then is returned as truncated.
const TREE_DEADLINE: Duration = Duration::from_secs(4);
/// Deeper elements are cut off, so a pathological chain cannot overflow the stack.
const MAX_DEPTH: usize = 100;
const SECURE_TEXT_FIELD: &str = "AXSecureTextField";

/// Read in one round-trip per element. `AXValue` is read separately, and only
/// once the element is known not to be a secure field; `AXChildren` is read
/// separately so that no more children are fetched than can be used.
const ATTRIBUTES: [&str; 8] = [
    "AXRole",
    "AXSubrole",
    "AXTitle",
    "AXDescription",
    "AXPosition",
    "AXSize",
    "AXEnabled",
    "AXFocused",
];
const ROLE: usize = 0;
const SUBROLE: usize = 1;
const TITLE: usize = 2;
const DESCRIPTION: usize = 3;
const POSITION: usize = 4;
const SIZE: usize = 5;
const ENABLED: usize = 6;
const FOCUSED: usize = 7;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Declared here because the crate's wrapper panics on NULL.
    fn AXUIElementCreateApplication(pid: i32) -> Option<NonNull<AXUIElement>>;
    /// Private but long-stable: the CGWindowID behind an AX window element.
    fn _AXUIElementGetWindow(element: &AXUIElement, window_id: *mut u32) -> AXError;
}

/// Reads trees and acts on the elements of the most recent one.
#[derive(Default)]
pub struct MacUiTree {
    live: Mutex<Live>,
}

/// The elements of the most recent tree. A handle is that tree's generation
/// in the high 32 bits and the element's index in the low 32, so a handle of
/// an older tree can never name an element of this one.
#[derive(Default)]
struct Live {
    /// Last generation handed out: trees read concurrently get distinct ones.
    issued: u32,
    generation: u32,
    elements: Vec<CFRetained<AXUIElement>>,
}

// SAFETY: an AXUIElement is an immutable CF object (a process id and a token),
// CF reference counting is atomic, and the accessibility client API may be
// called from any thread; the Mutex serialises access to the list itself.
unsafe impl Send for Live {}

fn handle(generation: u32, index: usize) -> ElementHandle {
    (u64::from(generation) << 32) | index as u64
}

impl Live {
    fn element(&self, handle: ElementHandle) -> PortResult<CFRetained<AXUIElement>> {
        let generation = (handle >> 32) as u32;
        let index = (handle & u64::from(u32::MAX)) as usize;
        let current = generation != 0 && generation == self.generation;
        current.then(|| self.elements.get(index).cloned()).flatten().ok_or_else(|| {
            "that element is from an earlier observe, so it may no longer be the same \
             element; call observe again"
                .to_string()
        })
    }
}

impl MacUiTree {
    fn live(&self) -> MutexGuard<'_, Live> {
        // The list stays consistent even if a holder panicked: it is only ever replaced whole.
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl UiTree for MacUiTree {
    fn tree(&self, pid: i32, window_id: u32, max_nodes: usize) -> PortResult<UiTreeSnapshot> {
        let app = application(pid)?;
        let window = find_window(&app, pid, window_id)?;
        let generation = {
            let mut live = self.live();
            live.issued = live.issued.wrapping_add(1).max(1);
            live.issued
        };
        let names: Vec<CFRetained<CFString>> =
            ATTRIBUTES.iter().map(|a| CFString::from_static_str(a)).collect();
        let mut walker = Walker {
            pid,
            names: CFArray::from_retained_objects(&names),
            value_name: CFString::from_static_str("AXValue"),
            children_name: CFString::from_static_str("AXChildren"),
            deadline: Instant::now() + TREE_DEADLINE,
            remaining: max_nodes,
            truncated: false,
            generation,
            elements: Vec::new(),
        };
        let root = walker.walk(window, 0)?;
        let mut live = self.live();
        live.generation = generation;
        live.elements = walker.elements;
        Ok(UiTreeSnapshot { root, truncated: walker.truncated })
    }
}

impl UiActions for MacUiTree {
    fn is_at(&self, element: ElementHandle, point: Point) -> PortResult<bool> {
        let element = self.live().element(element)?;
        let app = application(pid(&element).map_err(element_error)?)?;
        unsafe { app.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
        let mut hit: *const AXUIElement = ptr::null();
        // SAFETY: valid element; `hit` is a valid out-pointer.
        let status = unsafe { app.copy_element_at_position(point.x as f32, point.y as f32, NonNull::from(&mut hit)) };
        match status {
            AXError::Success => {}
            // Nothing of this app is at that point.
            AXError::NoValue => return Ok(false),
            status => return Err(element_error(status)),
        }
        // SAFETY: on success `hit` holds a +1 reference (Copy rule) or NULL.
        let Some(mut current) = NonNull::new(hit.cast_mut()).map(|h| unsafe { CFRetained::from_raw(h) }) else {
            return Ok(false);
        };
        // The hit is often a part of the element, like the label in a button.
        let parent = CFString::from_static_str("AXParent");
        for _ in 0..MAX_DEPTH {
            if *current == *element {
                return Ok(true);
            }
            match copy_attribute(&current, &parent).ok().flatten().map(|p| p.downcast::<AXUIElement>()) {
                Some(Ok(next)) => current = next,
                _ => return Ok(false),
            }
        }
        Ok(false)
    }

    fn frame(&self, element: ElementHandle) -> PortResult<Option<Rect>> {
        let element = self.live().element(element)?;
        let Some(mut visible) = frame(&element).map_err(element_error)? else { return Ok(None) };
        let (parent, role) = (CFString::from_static_str("AXParent"), CFString::from_static_str("AXRole"));
        let mut current = element;
        for _ in 0..MAX_DEPTH {
            let Some(next) = copy_attribute(&current, &parent).ok().flatten().and_then(|p| p.downcast().ok()) else {
                break;
            };
            let scroll_area = copy_attribute(&next, &role)
                .ok()
                .flatten()
                .is_some_and(|r| r.downcast_ref::<CFString>().is_some_and(|r| r.to_string() == "AXScrollArea"));
            // A scroll area shows only the part of its content inside its own frame.
            if let Some(area) = frame(&next).ok().flatten().filter(|_| scroll_area) {
                let Some(clipped) = intersection(visible, area) else { return Ok(None) };
                visible = clipped;
            }
            current = next;
        }
        Ok(Some(visible))
    }

    fn perform(&self, element: ElementHandle, action: UiAction) -> PortResult<bool> {
        let element = self.live().element(element)?;
        let name = CFString::from_static_str(match action {
            UiAction::Press => "AXPress",
            UiAction::ShowMenu => "AXShowMenu",
        });
        if !action_names(&element).map_err(element_error)?.contains(&name) {
            return Ok(false);
        }
        let answers = || copy_attribute(&element, &CFString::from_static_str("AXRole")).is_ok();
        match perform_action(&element, &name) {
            AXError::Success => Ok(true),
            // A press that opens a modal dialog (NSAlert.runModal, seen live)
            // returns only once the dialog closes, so the call times out with
            // CannotComplete although the press happened. An app handles
            // requests in order, so one that answers a read right afterwards
            // has taken the press: it is running what the press opened, or it
            // was merely slow and has performed it since.
            AXError::CannotComplete if answers() => Ok(true),
            AXError::CannotComplete => Err(
                "the app stopped answering, so it is unknown whether the element was pressed; \
                 call observe to check before trying again"
                    .to_string(),
            ),
            status => Err(element_error(status)),
        }
    }

    fn insert_text(&self, element: ElementHandle, text: &str, replace: bool) -> PortResult<bool> {
        let element = self.live().element(element)?;
        let attribute = CFString::from_static_str(if replace { "AXValue" } else { "AXSelectedText" });
        // An AppKit text field has a selection, and so a settable
        // AXSelectedText, only while it has focus.
        if !replace && flag(&element, "AXFocused") != Ok(Some(true)) && !focus_at_end(&element) {
            return Ok(false);
        }
        if !settable(&element, &attribute).map_err(element_error)? {
            return Ok(false);
        }
        set_attribute(&element, &attribute, &CFString::from_str(text)).map_err(element_error)?;
        Ok(true)
    }
}

/// Focuses a text element with the insertion point after its last
/// character, ready for AXSelectedText. Focusing alone selects all of an
/// AppKit field's text (seen live), and inserting would then replace it.
/// When this returns false, focus is where it was before.
fn focus_at_end(element: &AXUIElement) -> bool {
    let focused = CFString::from_static_str("AXFocused");
    // What can be checked without changing anything is checked first.
    let Some(end) = end_of_text(element) else { return false };
    if settable(element, &focused) != Ok(true) {
        return false;
    }
    // Whether the caret can then be placed is known only once it has focus.
    let previous = pid(element)
        .ok()
        .and_then(|pid| application(pid).ok())
        .and_then(|app| copy_attribute(&app, &CFString::from_static_str("AXFocusedUIElement")).ok().flatten())
        .and_then(|e| e.downcast::<AXUIElement>().ok());
    let ready = set_attribute(element, &focused, CFBoolean::new(true)).is_ok()
        && set_attribute(element, &CFString::from_static_str("AXSelectedTextRange"), &end).is_ok()
        && settable(element, &CFString::from_static_str("AXSelectedText")) == Ok(true);
    if !ready {
        let _ = match previous {
            Some(previous) => set_attribute(&previous, &focused, CFBoolean::new(true)),
            None => set_attribute(element, &focused, CFBoolean::new(false)),
        };
    }
    ready
}

/// An empty text range after the element's last character.
fn end_of_text(element: &AXUIElement) -> Option<CFRetained<AXValue>> {
    let length = copy_attribute(element, &CFString::from_static_str("AXNumberOfCharacters")).ok()??;
    let location = CFIndex::try_from(length.downcast_ref::<CFNumber>()?.as_i64()?).ok()?;
    let mut end = CFRange { location, length: 0 };
    // SAFETY: a CFRange payload for the CFRange kind.
    unsafe { AXValue::new(AXValueType::CFRange, NonNull::from(&mut end).cast()) }
}

fn pid(element: &AXUIElement) -> Result<i32, AXError> {
    let mut pid = 0;
    // SAFETY: valid element; `pid` is a valid out-pointer.
    match unsafe { element.pid(NonNull::from(&mut pid)) } {
        AXError::Success => Ok(pid),
        status => Err(status),
    }
}

/// The accessibility element of an app.
pub(super) fn application(pid: i32) -> PortResult<CFRetained<AXUIElement>> {
    // SAFETY: returns a +1 reference (Create rule) or NULL.
    unsafe { AXUIElementCreateApplication(pid) }
        .map(|app| unsafe { CFRetained::from_raw(app) })
        .ok_or_else(|| ax_error(AXError::Failure, pid))
}

pub(super) fn find_window(app: &AXUIElement, pid: i32, window_id: u32) -> PortResult<CFRetained<AXUIElement>> {
    let windows = copy_attribute(app, &CFString::from_static_str("AXWindows")).map_err(|e| ax_error(e, pid))?;
    elements(windows.as_deref().and_then(|w| w.downcast_ref::<CFArray>()))
        .into_iter()
        .find(|w| {
            let mut id = 0;
            // SAFETY: `w` is a valid element and `id` a valid out-pointer.
            let status = unsafe { _AXUIElementGetWindow(w, &mut id) };
            status == AXError::Success && id == window_id
        })
        .ok_or_else(|| {
            format!(
                "window {window_id} is not among its app's accessibility windows: it may have \
                 just closed (call list_windows), or the app does not expose it (call observe \
                 with tree false for a screenshot only)"
            )
        })
}

/// What one slot of a multiple-attribute read held.
#[derive(Debug, PartialEq)]
enum Attr {
    /// The element has no such attribute, or it has no value.
    Absent,
    Text(String),
    /// Unreadable, or of a type we did not expect.
    Other,
}

impl Attr {
    fn from_slot(slot: Option<&CFType>) -> Self {
        let Some(slot) = slot else { return Self::Other };
        if let Some(text) = slot.downcast_ref::<CFString>() {
            return Self::Text(text.to_string());
        }
        // The attribute exists but has no value.
        if slot.downcast_ref::<CFNull>().is_some() {
            return Self::Absent;
        }
        // With no options, a slot that could not be read holds the AXError.
        match slot.downcast_ref::<AXValue>().and_then(ax_value::<AXError>) {
            Some(AXError::NoValue | AXError::AttributeUnsupported) => Self::Absent,
            _ => Self::Other,
        }
    }

    fn text(self) -> String {
        match self {
            Self::Text(text) => text,
            _ => String::new(),
        }
    }
}

/// Fails closed: `AXValue` may be read only when the element is positively
/// known not to be a secure (password) field.
fn may_read_value(role: &Attr, subrole: &Attr) -> bool {
    let Attr::Text(role) = role else { return false };
    let subrole_ok = match subrole {
        Attr::Absent => true,
        Attr::Text(subrole) => subrole != SECURE_TEXT_FIELD,
        Attr::Other => false,
    };
    !role.is_empty() && role != SECURE_TEXT_FIELD && subrole_ok
}

struct Walker {
    pid: i32,
    names: CFRetained<CFArray<CFString>>,
    value_name: CFRetained<CFString>,
    children_name: CFRetained<CFString>,
    deadline: Instant,
    /// Elements that may still be read.
    remaining: usize,
    truncated: bool,
    generation: u32,
    /// Every element read, indexed by the low half of its handle.
    elements: Vec<CFRetained<AXUIElement>>,
}

impl Walker {
    /// Depth-first. Only the root failing fails the tree; any other element
    /// that fails becomes an empty node (which the domain drops).
    fn walk(&mut self, element: CFRetained<AXUIElement>, depth: usize) -> PortResult<UiNode> {
        self.remaining = self.remaining.saturating_sub(1);
        // One child more than can be visited tells whether the tree was cut short.
        let child_limit = if depth + 1 >= MAX_DEPTH { 1 } else { self.remaining.saturating_add(1) };
        let (mut node, children) = match self.read(&element, child_limit) {
            Ok(read) => read,
            Err(status) if depth == 0 || status == AXError::APIDisabled => {
                return Err(ax_error(status, self.pid));
            }
            Err(_) => {
                self.truncated = true;
                return Ok(UiNode::default());
            }
        };
        node.handle = handle(self.generation, self.elements.len());
        self.elements.push(element);
        for child in children {
            if self.remaining == 0 || depth + 1 >= MAX_DEPTH || Instant::now() >= self.deadline {
                self.truncated = true;
                break;
            }
            node.children.push(self.walk(child, depth + 1)?);
        }
        Ok(node)
    }

    /// The element itself and at most `child_limit` of its children.
    fn read(
        &self,
        element: &AXUIElement,
        child_limit: usize,
    ) -> Result<(UiNode, Vec<CFRetained<AXUIElement>>), AXError> {
        unsafe { element.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
        let mut out: *const CFArray = ptr::null();
        // SAFETY: valid element and attribute array; `out` is a valid out-pointer.
        let status = unsafe {
            element.copy_multiple_attribute_values(
                self.names.as_opaque(),
                AXCopyMultipleAttributeOptions::empty(),
                NonNull::from(&mut out),
            )
        };
        if status != AXError::Success {
            return Err(status);
        }
        // SAFETY: on success `out` holds a +1 array (Copy rule) or NULL.
        let values = NonNull::new(out.cast_mut())
            .map(|values| unsafe { CFRetained::from_raw(values) })
            .ok_or(AXError::Failure)?;
        // SAFETY: the result holds one CF object per requested attribute.
        let values = unsafe { values.cast_unchecked::<CFType>() };
        let get = |i| values.get(i);
        let attr = |i| Attr::from_slot(get(i).as_deref());
        let flag = |i| get(i).and_then(|v| v.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool));

        let (role, subrole) = (attr(ROLE), attr(SUBROLE));
        let value = if may_read_value(&role, &subrole) { self.value(element)? } else { String::new() };
        let position = get(POSITION).and_then(|v| v.downcast_ref::<AXValue>().and_then(ax_value::<CGPoint>));
        let size = get(SIZE).and_then(|v| v.downcast_ref::<AXValue>().and_then(ax_value::<CGSize>));
        let frame = position
            .zip(size)
            .map(|(p, s)| Rect { x: p.x, y: p.y, width: s.width, height: s.height });

        let node = UiNode {
            role: role.text(),
            title: attr(TITLE).text(),
            description: attr(DESCRIPTION).text(),
            value,
            frame,
            enabled: flag(ENABLED).unwrap_or(true),
            focused: flag(FOCUSED).unwrap_or(false),
            ..UiNode::default()
        };
        Ok((node, self.children(element, child_limit)?))
    }

    /// `AXValue` as text: strings as-is, numbers and booleans stringified.
    fn value(&self, element: &AXUIElement) -> Result<String, AXError> {
        let Some(value) = copy_attribute(element, &self.value_name)? else {
            return Ok(String::new());
        };
        Ok(if let Some(s) = value.downcast_ref::<CFString>() {
            s.to_string()
        } else if let Some(n) = value.downcast_ref::<CFNumber>() {
            // Three decimals: float32 values otherwise print as 0.18518517911434174.
            n.as_f64().map(|n| ((n * 1000.0).round() / 1000.0).to_string()).unwrap_or_default()
        } else if let Some(b) = value.downcast_ref::<CFBoolean>() {
            b.as_bool().to_string()
        } else {
            String::new()
        })
    }

    /// At most `limit` children, without fetching a possibly huge list.
    fn children(&self, element: &AXUIElement, limit: usize) -> Result<Vec<CFRetained<AXUIElement>>, AXError> {
        let limit = CFIndex::try_from(limit).unwrap_or(CFIndex::MAX);
        match self.copy_children(element, limit) {
            Err(AXError::NoValue | AXError::AttributeUnsupported) => Ok(Vec::new()),
            // Means either "no children" (index 0 out of range) or, in strict
            // implementations, "fewer children than `limit`": the count decides.
            Err(AXError::IllegalArgument) => {
                let mut count: CFIndex = 0;
                // SAFETY: valid element and name; `count` is a valid out-pointer.
                let status = unsafe { element.attribute_value_count(&self.children_name, NonNull::from(&mut count)) };
                match status {
                    AXError::Success if count > 0 => self.copy_children(element, count.min(limit)),
                    AXError::Success | AXError::NoValue | AXError::AttributeUnsupported => Ok(Vec::new()),
                    _ => Err(status),
                }
            }
            read => read,
        }
    }

    fn copy_children(&self, element: &AXUIElement, max: CFIndex) -> Result<Vec<CFRetained<AXUIElement>>, AXError> {
        let mut out: *const CFArray = ptr::null();
        // SAFETY: valid element and name; `out` is a valid out-pointer.
        let status = unsafe { element.copy_attribute_values(&self.children_name, 0, max, NonNull::from(&mut out)) };
        if status != AXError::Success {
            return Err(status);
        }
        // SAFETY: on success `out` holds a +1 array (Copy rule) or NULL.
        let children = NonNull::new(out.cast_mut()).map(|c| unsafe { CFRetained::from_raw(c) });
        Ok(elements(children.as_deref()))
    }
}

/// `Ok(None)` when the element has no such attribute or it has no value.
fn copy_attribute(element: &AXUIElement, name: &CFString) -> Result<Option<CFRetained<CFType>>, AXError> {
    unsafe { element.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
    let mut out: *const CFType = ptr::null();
    // SAFETY: valid element and name; `out` is a valid out-pointer.
    let status = unsafe { element.copy_attribute_value(name, NonNull::from(&mut out)) };
    if matches!(status, AXError::NoValue | AXError::AttributeUnsupported) {
        return Ok(None);
    }
    if status != AXError::Success {
        return Err(status);
    }
    // SAFETY: on success `out` holds a +1 reference (Copy rule) or NULL.
    Ok(NonNull::new(out.cast_mut()).map(|v| unsafe { CFRetained::from_raw(v) }))
}

/// Position and size in screen points, read now. `None` without a frame.
pub(super) fn frame(element: &AXUIElement) -> Result<Option<Rect>, AXError> {
    let read = |name| -> Result<Option<CFRetained<AXValue>>, AXError> {
        Ok(copy_attribute(element, &CFString::from_static_str(name))?.and_then(|v| v.downcast().ok()))
    };
    let position = read("AXPosition")?.and_then(|v| ax_value::<CGPoint>(&v));
    let size = read("AXSize")?.and_then(|v| ax_value::<CGSize>(&v));
    Ok(position.zip(size).map(|(p, s)| Rect { x: p.x, y: p.y, width: s.width, height: s.height }))
}

/// The overlap of two rectangles; `None` if they do not overlap.
fn intersection(a: Rect, b: Rect) -> Option<Rect> {
    let (x, y) = (a.x.max(b.x), a.y.max(b.y));
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    (right > x && bottom > y).then_some(Rect { x, y, width: right - x, height: bottom - y })
}

/// A boolean attribute; `None` if the element has no such attribute.
pub(super) fn flag(element: &AXUIElement, name: &'static str) -> Result<Option<bool>, AXError> {
    let value = copy_attribute(element, &CFString::from_static_str(name))?;
    Ok(value.and_then(|v| v.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool)))
}

pub(super) fn set_attribute(element: &AXUIElement, name: &CFString, value: &CFType) -> Result<(), AXError> {
    unsafe { element.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
    // SAFETY: valid element, name and value.
    match unsafe { element.set_attribute_value(name, value) } {
        AXError::Success => Ok(()),
        status => Err(status),
    }
}

/// False also when the element has no such attribute.
fn settable(element: &AXUIElement, name: &CFString) -> Result<bool, AXError> {
    unsafe { element.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
    let mut settable = 0;
    // SAFETY: valid element and name; `settable` is a valid out-pointer.
    match unsafe { element.is_attribute_settable(name, NonNull::from(&mut settable)) } {
        AXError::Success => Ok(settable != 0),
        AXError::NoValue | AXError::AttributeUnsupported => Ok(false),
        status => Err(status),
    }
}

fn action_names(element: &AXUIElement) -> Result<Vec<CFRetained<CFString>>, AXError> {
    unsafe { element.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
    let mut out: *const CFArray = ptr::null();
    // SAFETY: valid element; `out` is a valid out-pointer.
    let status = unsafe { element.copy_action_names(NonNull::from(&mut out)) };
    if status != AXError::Success {
        return Err(status);
    }
    // SAFETY: on success `out` holds a +1 array (Copy rule) or NULL.
    let Some(names) = NonNull::new(out.cast_mut()).map(|n| unsafe { CFRetained::<CFArray>::from_raw(n) }) else {
        return Ok(Vec::new());
    };
    // SAFETY: action names are CF objects (CFStrings).
    let names = unsafe { names.cast_unchecked::<CFType>() };
    Ok(names.iter().filter_map(|n| n.downcast().ok()).collect())
}

pub(super) fn perform_action(element: &AXUIElement, name: &CFString) -> AXError {
    unsafe { element.set_messaging_timeout(MESSAGING_TIMEOUT_SECS) };
    // SAFETY: valid element and action name.
    unsafe { element.perform_action(name) }
}

/// The elements in an array-valued attribute such as `AXWindows`.
fn elements(array: Option<&CFArray>) -> Vec<CFRetained<AXUIElement>> {
    let Some(array) = array else { return Vec::new() };
    // SAFETY: CFArrays returned by accessibility attributes hold CF objects.
    let array = unsafe { array.cast_unchecked::<CFType>() };
    array.iter().filter_map(|e| e.downcast().ok()).collect()
}

/// A type an `AXValue` can be unpacked into.
///
/// # Safety
/// `Self` must have the memory layout of the C type that `KIND` stands for.
unsafe trait AxPayload: Copy {
    const KIND: AXValueType;
    const EMPTY: Self;
}

unsafe impl AxPayload for CGPoint {
    const KIND: AXValueType = AXValueType::CGPoint;
    const EMPTY: Self = CGPoint { x: 0.0, y: 0.0 };
}

unsafe impl AxPayload for CGSize {
    const KIND: AXValueType = AXValueType::CGSize;
    const EMPTY: Self = CGSize { width: 0.0, height: 0.0 };
}

unsafe impl AxPayload for AXError {
    const KIND: AXValueType = AXValueType::AXError;
    const EMPTY: Self = AXError::Success;
}

/// `None` unless the value holds a `T`.
fn ax_value<T: AxPayload>(value: &AXValue) -> Option<T> {
    let mut out = T::EMPTY;
    // SAFETY: `T` matches `T::KIND` (see `AxPayload`); AXValueGetValue writes
    // only if the stored type is `T::KIND`.
    unsafe { value.value(T::KIND, NonNull::from(&mut out).cast()) }.then_some(out)
}

const PERMISSION_MISSING: &str = "Accessibility permission is missing. The user must enable it \
     for the app running this server in System Settings > Privacy & Security > Accessibility, \
     then restart that app. Until then, call observe with tree false for a screenshot only.";

pub(super) fn ax_error(status: AXError, pid: i32) -> String {
    match status {
        AXError::APIDisabled => PERMISSION_MISSING.to_string(),
        AXError::InvalidUIElement => "the window or its app is gone; call list_windows".to_string(),
        AXError::CannotComplete => format!(
            "the app (pid {pid}) is not answering accessibility requests, or it has quit; call \
             list_windows to check, or observe with tree false for a screenshot only"
        ),
        _ => format!(
            "reading the accessibility tree of the app (pid {pid}) failed (AXError {}); call \
             observe with tree false for a screenshot only",
            status.0
        ),
    }
}

/// For a failed request about one element of the latest tree.
fn element_error(status: AXError) -> String {
    match status {
        AXError::APIDisabled => PERMISSION_MISSING.to_string(),
        AXError::InvalidUIElement => {
            "that element no longer exists (its window changed or closed); call observe again".to_string()
        }
        AXError::CannotComplete => "the app is not answering accessibility requests; call observe to \
                                    check its state"
            .to_string(),
        _ => format!("the accessibility request failed (AXError {}); call observe again", status.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Attr {
        Attr::Text(s.to_string())
    }

    fn error_slot(mut error: AXError) -> CFRetained<AXValue> {
        // SAFETY: an AXError payload for the AXError kind.
        unsafe { AXValue::new(AXValueType::AXError, NonNull::from(&mut error).cast()) }.unwrap()
    }

    #[test]
    fn slots_are_classified_like_copy_multiple_attribute_values_fills_them() {
        let string = CFString::from_static_str("AXStandardWindow");
        assert_eq!(Attr::from_slot(Some(&string)), text("AXStandardWindow"));
        let null: &CFType = unsafe { objc2_core_foundation::kCFNull }.unwrap();
        assert_eq!(Attr::from_slot(Some(null)), Attr::Absent);
        assert_eq!(Attr::from_slot(Some(&error_slot(AXError::NoValue))), Attr::Absent);
        assert_eq!(Attr::from_slot(Some(&error_slot(AXError::AttributeUnsupported))), Attr::Absent);
        assert_eq!(Attr::from_slot(Some(&error_slot(AXError::CannotComplete))), Attr::Other);
        assert_eq!(Attr::from_slot(None), Attr::Other);
    }

    #[test]
    fn value_is_read_only_for_elements_known_not_to_be_secure() {
        assert!(may_read_value(&text("AXTextField"), &Attr::Absent), "ordinary field");
        assert!(may_read_value(&text("AXButton"), &text("AXCloseButton")));
        assert!(!may_read_value(&text(SECURE_TEXT_FIELD), &Attr::Absent), "secure role");
        assert!(!may_read_value(&text("AXTextField"), &text(SECURE_TEXT_FIELD)), "secure subrole");
        assert!(!may_read_value(&text("AXTextField"), &Attr::Other), "unreadable subrole");
        assert!(!may_read_value(&Attr::Other, &Attr::Absent), "unreadable role");
        assert!(!may_read_value(&Attr::Absent, &Attr::Absent), "no role");
        assert!(!may_read_value(&text(""), &Attr::Absent), "empty role");
    }

    #[test]
    fn intersection_keeps_only_the_overlap() {
        let rect = |x, y, width, height| Rect { x, y, width, height };
        let area = rect(0.0, 100.0, 400.0, 200.0);
        // A document view far taller than its scroll area.
        assert_eq!(intersection(rect(0.0, -500.0, 380.0, 2000.0), area), Some(rect(0.0, 100.0, 380.0, 200.0)));
        assert_eq!(intersection(rect(10.0, 120.0, 50.0, 20.0), area), Some(rect(10.0, 120.0, 50.0, 20.0)), "inside");
        assert_eq!(intersection(rect(10.0, 350.0, 50.0, 20.0), area), None, "scrolled out");
        assert_eq!(intersection(rect(10.0, 300.0, 50.0, 20.0), area), None, "only touching");
    }

    #[test]
    fn handles_resolve_only_within_the_tree_that_issued_them() {
        let element = application(std::process::id() as i32).unwrap();
        let live = Live { issued: 3, generation: 2, elements: vec![element.clone(), element] };
        assert!(live.element(handle(2, 1)).is_ok());
        assert!(live.element(handle(1, 1)).is_err(), "earlier tree");
        assert!(live.element(handle(3, 1)).is_err(), "tree still being read");
        assert!(live.element(handle(2, 2)).is_err(), "no such element");
        assert!(live.element(0).is_err(), "an element that could not be read");
    }
}
