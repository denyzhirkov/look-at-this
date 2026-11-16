//! The element tree of one window as the agent last saw it.

use std::fmt::Write;

use crate::domain::viewport::Viewport;
use crate::ports::{ElementHandle, Rect, UiNode};

const MAX_TEXT_CHARS: usize = 80;
/// Deeper elements are drawn at this depth, so output grows linearly.
const MAX_INDENT: usize = 16;

#[derive(Debug)]
pub struct Element {
    /// Shown to the agent as `e<id>`. Never reused across snapshots, so a ref
    /// from an older observe cannot silently resolve to a different element.
    pub id: u64,
    pub handle: ElementHandle,
    pub depth: usize,
    pub role: String,
    pub label: String,
    pub value: String,
    /// Points relative to the window's top-left.
    pub frame: Option<Rect>,
    pub enabled: bool,
    pub focused: bool,
}

#[derive(Debug)]
pub struct Snapshot {
    pub window_id: u32,
    /// The process the elements belong to: window ids can be reused.
    pub pid: i32,
    pub elements: Vec<Element>,
}

#[derive(Debug, PartialEq)]
pub enum RefError {
    Malformed,
    /// Not part of the latest observe of this window.
    Stale,
}

impl Snapshot {
    /// Flattens the tree, numbering elements from `first_id`. Unlabelled
    /// containers are dropped and their children take their place.
    pub fn build(window_id: u32, pid: i32, window_origin: (f64, f64), root: UiNode, first_id: u64) -> Self {
        let mut elements = Vec::new();
        flatten(root, 0, window_origin, first_id, &mut elements);
        Self { window_id, pid, elements }
    }

    pub fn resolve(&self, window_id: u32, pid: i32, reference: &str) -> Result<&Element, RefError> {
        let id: u64 = reference
            .strip_prefix('e')
            .and_then(|n| n.parse().ok())
            .ok_or(RefError::Malformed)?;
        if (window_id, pid) != (self.window_id, self.pid) {
            return Err(RefError::Stale);
        }
        let first = self.elements.first().map_or(0, |e| e.id);
        id.checked_sub(first)
            .and_then(|index| self.elements.get(index as usize))
            .ok_or(RefError::Stale)
    }

    /// One line per element: `e7 button "Save" [10,20 80x24] disabled`.
    /// Coordinates are image pixels of `viewport`.
    pub fn render(&self, viewport: &Viewport) -> String {
        let mut out = String::new();
        for e in &self.elements {
            let _ = write!(out, "{}e{} {}", "  ".repeat(e.depth.min(MAX_INDENT)), e.id, e.role);
            if !e.label.is_empty() {
                let _ = write!(out, " \"{}\"", e.label);
            }
            if !e.value.is_empty() {
                let _ = write!(out, " =\"{}\"", e.value);
            }
            if let Some(frame) = e.frame {
                let r = viewport.to_image(frame);
                let _ = write!(
                    out,
                    " [{},{} {}x{}]",
                    r.x.round(),
                    r.y.round(),
                    r.width.round(),
                    r.height.round()
                );
            }
            if !e.enabled {
                out.push_str(" disabled");
            }
            if e.focused {
                out.push_str(" focused");
            }
            out.push('\n');
        }
        out
    }
}

fn flatten(node: UiNode, depth: usize, origin: (f64, f64), first_id: u64, out: &mut Vec<Element>) {
    let label = if node.title.is_empty() { &node.description } else { &node.title };
    let is_plain_container = matches!(node.role.as_str(), "AXGroup" | "AXUnknown" | "")
        && label.is_empty()
        && node.value.is_empty();

    let child_depth = if is_plain_container {
        depth
    } else {
        out.push(Element {
            id: first_id + out.len() as u64,
            handle: node.handle,
            depth,
            role: one_line(node.role.strip_prefix("AX").unwrap_or(&node.role)).to_lowercase(),
            label: one_line(label),
            value: one_line(&node.value),
            frame: node
                .frame
                .map(|f| Rect { x: f.x - origin.0, y: f.y - origin.1, ..f }),
            enabled: node.enabled,
            focused: node.focused,
        });
        depth + 1
    };
    for child in node.children {
        flatten(child, child_depth, origin, first_id, out);
    }
}

/// Collapses whitespace, escapes quotes and truncates, so text coming from an
/// app stays on one unambiguous line and cannot imitate other lines.
pub fn one_line(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ").replace('"', "'");
    if collapsed.chars().count() > MAX_TEXT_CHARS {
        let mut cut: String = collapsed.chars().take(MAX_TEXT_CHARS).collect();
        cut.push('…');
        cut
    } else {
        collapsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(role: &str, title: &str, children: Vec<UiNode>) -> UiNode {
        UiNode {
            role: role.to_string(),
            title: title.to_string(),
            enabled: true,
            children,
            ..UiNode::default()
        }
    }

    fn sample() -> UiNode {
        let mut save = node("AXButton", "Save", vec![]);
        save.handle = 42;
        save.frame = Some(Rect { x: 110.0, y: 220.0, width: 80.0, height: 24.0 });
        save.enabled = false;
        let mut field = node("AXTextArea", "", vec![]);
        field.value = "line one\nline \"two\"".to_string();
        field.focused = true;
        node("AXWindow", "Doc", vec![node("AXGroup", "", vec![save]), field])
    }

    #[test]
    fn unlabelled_groups_are_dropped_and_children_hoisted() {
        let s = Snapshot::build(1, 9, (100.0, 200.0), sample(), 5);
        let viewport = Viewport::fit(Rect { x: 0.0, y: 0.0, width: 400.0, height: 300.0 }, 1.0, 1280);
        assert_eq!(
            s.render(&viewport),
            "e5 window \"Doc\"\n  \
             e6 button \"Save\" [10,20 80x24] disabled\n  \
             e7 textarea =\"line one line 'two'\" focused\n"
        );
    }

    #[test]
    fn refs_resolve_only_within_their_snapshot() {
        let s = Snapshot::build(1, 9, (100.0, 200.0), sample(), 5);
        let handle = |window, reference| s.resolve(window, 9, reference).map(|e| e.handle);
        assert_eq!(handle(1, "e6"), Ok(42));
        assert_eq!(s.resolve(1, 8, "e6").map(|e| e.handle), Err(RefError::Stale), "another process");
        assert_eq!(handle(1, "e4"), Err(RefError::Stale), "older than this snapshot");
        assert_eq!(handle(1, "e8"), Err(RefError::Stale), "not issued yet");
        assert_eq!(handle(2, "e6"), Err(RefError::Stale), "another window");
        assert_eq!(handle(1, "save"), Err(RefError::Malformed));
    }

    #[test]
    fn long_text_is_truncated() {
        let mut n = node("AXStaticText", "", vec![]);
        n.value = "x".repeat(200);
        let s = Snapshot::build(1, 9, (0.0, 0.0), n, 1);
        assert_eq!(s.elements[0].value.chars().count(), MAX_TEXT_CHARS + 1);
    }
}
