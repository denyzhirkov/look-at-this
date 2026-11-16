//! Accessibility tree of a window via the AXUIElement API.

use std::ptr::{self, NonNull};
use std::time::{Duration, Instant};

use objc2_application_services::{AXCopyMultipleAttributeOptions, AXError, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFIndex, CFNull, CFNumber, CFRetained, CFString, CFType, CGPoint, CGSize,
};

use crate::ports::{PortResult, Rect, UiNode, UiTree, UiTreeSnapshot};

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

pub struct MacUiTree;

impl UiTree for MacUiTree {
    fn tree(&self, pid: i32, window_id: u32, max_nodes: usize) -> PortResult<UiTreeSnapshot> {
        // SAFETY: returns a +1 reference (Create rule) or NULL.
        let app = unsafe { AXUIElementCreateApplication(pid) }
            .map(|app| unsafe { CFRetained::from_raw(app) })
            .ok_or_else(|| ax_error(AXError::Failure, pid))?;
        let window = find_window(&app, pid, window_id)?;
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
        };
        let root = walker.walk(&window, 0)?;
        Ok(UiTreeSnapshot { root, truncated: walker.truncated })
    }
}

fn find_window(app: &AXUIElement, pid: i32, window_id: u32) -> PortResult<CFRetained<AXUIElement>> {
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
}

impl Walker {
    /// Depth-first. Only the root failing fails the tree; any other element
    /// that fails becomes an empty node (which the domain drops).
    fn walk(&mut self, element: &AXUIElement, depth: usize) -> PortResult<UiNode> {
        self.remaining = self.remaining.saturating_sub(1);
        // One child more than can be visited tells whether the tree was cut short.
        let child_limit = if depth + 1 >= MAX_DEPTH { 1 } else { self.remaining.saturating_add(1) };
        let (mut node, children) = match self.read(element, child_limit) {
            Ok(read) => read,
            Err(status) if depth == 0 || status == AXError::APIDisabled => {
                return Err(ax_error(status, self.pid));
            }
            Err(_) => {
                self.truncated = true;
                return Ok(UiNode::default());
            }
        };
        for child in children {
            if self.remaining == 0 || depth + 1 >= MAX_DEPTH || Instant::now() >= self.deadline {
                self.truncated = true;
                break;
            }
            node.children.push(self.walk(&child, depth + 1)?);
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
            children: Vec::new(),
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

fn ax_error(status: AXError, pid: i32) -> String {
    match status {
        AXError::APIDisabled => "Accessibility permission is missing. The user must enable it for \
             the app running this server in System Settings > Privacy & Security > Accessibility, \
             then restart that app. Until then, call observe with tree false for a screenshot only."
            .to_string(),
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
}
