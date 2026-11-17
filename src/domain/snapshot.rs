//! The element tree of one window as the agent last saw it.

use std::fmt::{self, Write};

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
    /// Label and value in full and lower-cased, for filters: the shown ones
    /// are cut short and reworded, so text past the cut would never match.
    pub full_label: String,
    pub full_value: String,
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

    /// One line per element `filter` matches: `e7 button "Save" [10,20 80x24]
    /// disabled`, then a line saying how many are shown if it hid any.
    /// Coordinates are image pixels of `viewport`.
    pub fn render(&self, viewport: &Viewport, filter: &Filter) -> String {
        let mut out = String::new();
        let mut shown = 0;
        for e in self.elements.iter().filter(|e| filter.matches(e)) {
            shown += 1;
            // Matches are listed without their parents, so indenting them
            // would suggest a nesting that is not there.
            let indent = if filter.is_flat() { 0 } else { e.depth.min(MAX_INDENT) };
            let _ = write!(out, "{}e{} {}", "  ".repeat(indent), e.id, e.role);
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
        let total = self.elements.len();
        if shown == 0 && total > 0 {
            let _ = writeln!(out, "(no element matches the filter; the tree has {total})");
        } else if shown < total {
            let _ = writeln!(out, "({shown} of {total} elements shown; the filter hides the rest)");
        }
        out
    }
}

/// Rendered roles of the controls an agent can act on. Tabs are radio
/// buttons to accessibility, and search and password fields are text fields.
const INTERACTIVE_ROLES: &[&str] = &[
    "button",
    "checkbox",
    "radiobutton",
    "textfield",
    "textarea",
    "popupbutton",
    "menubutton",
    "combobox",
    "slider",
    "link",
    "menuitem",
    "menubaritem",
    "disclosuretriangle",
    "incrementor",
    "colorwell",
];

/// Which elements a rendered tree shows; every condition given must hold.
#[derive(Debug, Default)]
pub struct Filter {
    find: Option<String>,
    role: Option<String>,
    interactive: bool,
    max_depth: Option<usize>,
}

impl Filter {
    /// `find` is looked for in the label or value, `role` must equal the
    /// rendered role; both ignore case, and an empty string counts as absent.
    /// `max_depth` 0 is the window element alone.
    pub fn new(find: Option<String>, role: Option<String>, interactive: bool, max_depth: Option<usize>) -> Self {
        let given = |text: Option<String>| text.filter(|t| !t.is_empty()).map(|t| t.to_lowercase());
        Self { find: given(find), role: given(role), interactive, max_depth }
    }

    /// Whether it names what to look for, by text or role.
    pub fn names_something(&self) -> bool {
        self.find.is_some() || self.role.is_some()
    }

    fn is_flat(&self) -> bool {
        self.names_something() || self.interactive
    }

    pub fn matches(&self, e: &Element) -> bool {
        // The shown text too, so text copied from a tree line matches even
        // where it was put on one line.
        let has = |f: &String| {
            [&e.full_label, &e.full_value].iter().any(|t| t.contains(f))
                || [&e.label, &e.value].iter().any(|t| t.to_lowercase().contains(f))
        };
        self.find
            .as_ref()
            .is_none_or(has)
            && self.role.as_ref().is_none_or(|r| e.role == *r)
            && (!self.interactive || INTERACTIVE_ROLES.contains(&e.role.as_str()))
            && self.max_depth.is_none_or(|d| e.depth <= d)
    }
}

impl fmt::Display for Filter {
    /// `find "save" role "button"`, for messages to the agent.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = [("find", &self.find), ("role", &self.role)]
            .into_iter()
            .filter_map(|(name, text)| text.as_ref().map(|t| format!("{name} \"{}\"", one_line(t))))
            .collect();
        write!(f, "{}", parts.join(" "))
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
            full_label: label.to_lowercase(),
            full_value: node.value.to_lowercase(),
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
            s.render(&viewport, &Filter::default()),
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

    /// e1 window, e2 toolbar, e3 button "Save", e4 checkbox, e5 textarea
    /// holding "SAVED draft", e6 static text; depths 0, 1, 2, 2, 1, 1.
    fn rendered(filter: Filter) -> String {
        let mut field = node("AXTextArea", "", vec![]);
        field.value = "SAVED draft".to_string();
        let toolbar = node("AXToolbar", "Tools", vec![node("AXButton", "Save", vec![]), node("AXCheckBox", "Bold", vec![])]);
        let root = node("AXWindow", "Doc", vec![toolbar, field, node("AXStaticText", "Ruler", vec![])]);
        let viewport = Viewport::fit(Rect { x: 0.0, y: 0.0, width: 400.0, height: 300.0 }, 1.0, 1280);
        Snapshot::build(1, 9, (0.0, 0.0), root, 1).render(&viewport, &filter)
    }

    fn filter(find: &str, role: &str, interactive: bool, max_depth: Option<usize>) -> Filter {
        Filter::new(Some(find.to_string()), Some(role.to_string()), interactive, max_depth)
    }

    #[test]
    fn find_matches_label_or_value_ignoring_case_and_lists_flat() {
        assert_eq!(
            rendered(filter("save", "", false, None)),
            "e3 button \"Save\"\n\
             e5 textarea =\"SAVED draft\"\n\
             (2 of 6 elements shown; the filter hides the rest)\n"
        );
    }

    #[test]
    fn role_matches_the_rendered_role_ignoring_case() {
        assert_eq!(
            rendered(filter("", "Button", false, None)),
            "e3 button \"Save\"\n(1 of 6 elements shown; the filter hides the rest)\n"
        );
    }

    #[test]
    fn interactive_keeps_only_controls() {
        assert_eq!(
            rendered(filter("", "", true, None)),
            "e3 button \"Save\"\n\
             e4 checkbox \"Bold\"\n\
             e5 textarea =\"SAVED draft\"\n\
             (3 of 6 elements shown; the filter hides the rest)\n"
        );
    }

    #[test]
    fn max_depth_alone_keeps_the_indentation() {
        assert_eq!(
            rendered(filter("", "", false, Some(1))),
            "e1 window \"Doc\"\n  \
             e2 toolbar \"Tools\"\n  \
             e5 textarea =\"SAVED draft\"\n  \
             e6 statictext \"Ruler\"\n\
             (4 of 6 elements shown; the filter hides the rest)\n"
        );
        assert_eq!(rendered(filter("", "", false, Some(0))).lines().next(), Some("e1 window \"Doc\""));
    }

    #[test]
    fn conditions_combine_and_a_filter_hiding_nothing_adds_no_line() {
        assert_eq!(
            rendered(filter("SAVE", "", true, Some(1))),
            "e5 textarea =\"SAVED draft\"\n(1 of 6 elements shown; the filter hides the rest)\n"
        );
        assert_eq!(rendered(filter("", "", false, Some(5))), rendered(Filter::default()));
        assert_eq!(rendered(filter("", "", false, None)), rendered(Filter::default()), "empty strings are absent");
        assert!(!filter("", "", true, Some(1)).names_something());
    }

    #[test]
    fn no_match_is_said_plainly() {
        assert_eq!(
            rendered(filter("nowhere", "", false, None)),
            "(no element matches the filter; the tree has 6)\n"
        );
    }

    #[test]
    fn find_looks_at_the_whole_text_not_only_what_is_shown() {
        let mut long = node("AXTextArea", "", vec![]);
        long.value = format!("{}Invoice  \"42\"", "x".repeat(150));
        let mut short = node("AXTextArea", "", vec![]);
        short.value = "line one\nline \"two\"".to_string();
        let s = Snapshot::build(1, 9, (0.0, 0.0), node("AXWindow", "", vec![long, short]), 1);
        let found = |find: &str, index: usize| {
            Filter::new(Some(find.to_string()), None, false, None).matches(&s.elements[index])
        };
        assert!(found("invoice  \"42\"", 1), "past the cut, with two spaces and quotes");
        assert!(!found("invoice 43", 1));
        assert!(found("one line 'two'", 2), "as the tree line shows it");
        assert!(found("line \"two\"", 2), "as the app has it");
    }

    #[test]
    fn long_text_is_truncated() {
        let mut n = node("AXStaticText", "", vec![]);
        n.value = "x".repeat(200);
        let s = Snapshot::build(1, 9, (0.0, 0.0), n, 1);
        assert_eq!(s.elements[0].value.chars().count(), MAX_TEXT_CHARS + 1);
    }
}
