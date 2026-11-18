//! MCP server: thin handlers over the domain.

use std::error::Error;
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::service::RequestContext;
use rmcp::transport::stdio;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::config;
use crate::domain::doctor;
use crate::domain::policy::AccessPolicy;
use crate::domain::actions::{At, Mode};
use crate::domain::drag::drag_end;
use crate::domain::session::{ListedWindow, ObserveRequest, Session, Shot, WaitRequest};
use crate::domain::snapshot::{Filter, one_line};
use crate::ports::{Button, Permissions};

const DEFAULT_MAX_EDGE: u32 = 1280;
const DEFAULT_WAIT_MS: u64 = 5000;
const MAX_WAIT_MS: u64 = 30_000;
const WAIT_POLL: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub struct Server {
    permissions: Arc<dyn Permissions>,
    session: Arc<Session>,
    config_path: PathBuf,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum ScreenshotMode {
    None,
    Window,
    Region,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ObserveParams {
    /// Window id from list_windows.
    window_id: u32,
    /// "window" (default): image of the whole window. "none": no image, the
    /// cheapest option when only the element tree is needed. "region": a
    /// magnified image of the element given in `ref`, for reading small details.
    screenshot: Option<ScreenshotMode>,
    /// Include the element tree (default true). Ignored for "region".
    tree: Option<bool>,
    /// Element ref such as "e12", from the latest observe. Required for "region".
    #[serde(rename = "ref")]
    reference: Option<String>,
    /// Longest side of the image in pixels (default 1280). Larger windows are
    /// scaled down, and tree coordinates are scaled with them.
    max_edge: Option<u32>,
    /// Print only elements whose label or value contains this text, ignoring case.
    find: Option<String>,
    /// Print only elements of this role, e.g. "button", "textfield", "textarea".
    role: Option<String>,
    /// Print only controls that can be acted on: buttons, checkboxes, text
    /// fields, pop-ups, sliders, links, menu items and the like (default false).
    interactive: Option<bool>,
    /// Do not print elements nested deeper than this; 0 is the window alone.
    max_depth: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct WaitForParams {
    /// Window id from list_windows.
    window_id: u32,
    /// Text to wait for in an element's label or value, ignoring case.
    find: Option<String>,
    /// Role to wait for, e.g. "button" or "sheet". With find, both must match.
    role: Option<String>,
    /// Wait until no element matches instead (default false).
    gone: Option<bool>,
    /// How long to wait in milliseconds (default 5000, at most 30000).
    timeout_ms: Option<u64>,
}

#[derive(Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum InputMode {
    Auto,
    Background,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum MouseButton {
    Left,
    Right,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ClickParams {
    /// Window id from list_windows.
    window_id: u32,
    /// Element to click, such as "e12" from the latest observe. Preferred
    /// over x,y: it can usually be clicked without moving the user's pointer.
    #[serde(rename = "ref")]
    reference: Option<String>,
    /// Pixel of the latest window screenshot, for things the tree lacks.
    /// Always uses the real pointer.
    x: Option<f64>,
    y: Option<f64>,
    /// "left" (default) or "right".
    button: Option<MouseButton>,
    /// Double click (default false). Always uses the real pointer.
    double: Option<bool>,
    /// "auto" (default): accessibility if possible, else the real pointer,
    /// which brings the app to the front. "background": accessibility only;
    /// fails rather than disturb the user.
    mode: Option<InputMode>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct TypeTextParams {
    /// Window id from list_windows.
    window_id: u32,
    /// Element to type into, such as "e12". Omit to type into whatever has
    /// focus in that window (real keys only).
    #[serde(rename = "ref")]
    reference: Option<String>,
    text: String,
    /// Replace the element's whole content instead of inserting at the
    /// cursor (default false).
    replace: Option<bool>,
    /// "auto" (default) or "background"; see click.
    mode: Option<InputMode>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct PressKeyParams {
    /// Window id from list_windows.
    window_id: u32,
    /// A key with optional modifiers joined by "+": "enter", "cmd+s",
    /// "cmd+shift+z", "escape", "down", "f5". Modifiers: cmd, ctrl, alt, shift.
    keys: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ScrollParams {
    /// Window id from list_windows.
    window_id: u32,
    /// Element to scroll over, such as "e12". With neither ref nor x,y the
    /// middle of the window is used.
    #[serde(rename = "ref")]
    reference: Option<String>,
    /// Pixel of the latest window screenshot to scroll over.
    x: Option<f64>,
    y: Option<f64>,
    /// Lines to scroll down (negative: up). Default 0.
    dy: Option<i32>,
    /// Lines to scroll right (negative: left). Default 0.
    dx: Option<i32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct DragParams {
    /// Window id from list_windows. Both ends must lie inside this window.
    window_id: u32,
    /// Element to start at (its visible centre), such as "e12" from the latest observe.
    from_ref: Option<String>,
    /// Or a pixel of the latest window screenshot to start at.
    from_x: Option<f64>,
    from_y: Option<f64>,
    /// Element to end at (its visible centre).
    to_ref: Option<String>,
    /// Or a pixel of the latest window screenshot to end at.
    to_x: Option<f64>,
    to_y: Option<f64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
enum ControlValue {
    Text(String),
    Number(f64),
}

#[derive(Deserialize, schemars::JsonSchema)]
struct SetValueParams {
    /// Window id from list_windows.
    window_id: u32,
    /// The control, such as "e12" from the latest observe.
    #[serde(rename = "ref")]
    reference: String,
    /// "on" or "off" for a checkbox, switch or radio button (a radio button
    /// only "on"); a number for a slider or stepper; an option's title for a
    /// pop-up button.
    value: ControlValue,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct MenuParams {
    /// Window id from list_windows.
    window_id: u32,
    /// Menu titles joined by ">", e.g. "Format > Font > Bold". Omit to list
    /// the menu bar. Titles match ignoring case and a trailing "…"; a title
    /// containing ">" cannot be named.
    path: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ActivateParams {
    /// Window id from list_windows.
    window_id: u32,
}

/// `ref`, or `x` with `y`, or neither.
fn target(reference: Option<String>, x: Option<f64>, y: Option<f64>) -> Result<Option<At>, String> {
    match (reference, x, y) {
        (Some(reference), None, None) => Ok(Some(At::Element(reference))),
        (None, Some(x), Some(y)) => Ok(Some(At::Pixel { x, y })),
        (None, None, None) => Ok(None),
        _ => Err("pass either `ref` or both `x` and `y`, not a mix".to_string()),
    }
}

fn mode(mode: Option<InputMode>) -> Mode {
    match mode.unwrap_or(InputMode::Auto) {
        InputMode::Auto => Mode::Auto,
        InputMode::Background => Mode::Background,
    }
}

#[tool_router]
impl Server {
    pub fn new(permissions: Arc<dyn Permissions>, session: Session, config_path: PathBuf) -> Self {
        Self { permissions, session: Arc::new(session), config_path }
    }

    #[tool(
        description = "Check whether this server can see and control apps: macOS permissions, \
                       the list of allowed apps, and what the user must fix. Call it first."
    )]
    fn doctor(&self) -> Result<CallToolResult, ErrorData> {
        // Re-read on every call so `look-at-this allow` applies without a restart.
        let config = config::load(&self.config_path).map_err(|e| e.to_string());
        let report = doctor::report(
            self.permissions.as_ref(),
            &self.config_path,
            config,
            self.session.input_paused(),
        );
        Ok(CallToolResult::success(vec![ContentBlock::json(report)?]))
    }

    #[tool(
        description = "List the open windows of allowed apps, with the ids the other tools take. \
                       Each line is `id app (bundle) \"title\" WxH`, followed by state words when \
                       they apply: `frontmost` (its app is the front app), `thumbnail` (only a \
                       Stage Manager thumbnail is shown, because its app is in the background), \
                       `off-screen` (minimised, its app hidden, or on another Space). A thumbnail \
                       or off-screen window must be brought forward with activate before observe \
                       or any action works on it."
    )]
    fn list_windows(&self) -> CallToolResult {
        let windows = match self.policy().and_then(|p| self.session.list_windows(&p).map_err(|e| e.to_string())) {
            Ok(windows) => windows,
            Err(message) => return failure(message),
        };
        if windows.is_empty() {
            return text("No windows of allowed apps are open. Call doctor to see which apps are allowed.");
        }
        text(windows.iter().map(window_line).collect::<String>())
    }

    #[tool(
        description = "Bring a window and its app to the front, as a person would: un-minimises \
                       it and turns a Stage Manager thumbnail into the full window. Needed before \
                       observe or any action on a window that list_windows marks `thumbnail` or \
                       `off-screen`. It interrupts what the user sees, so use it only for that."
    )]
    fn activate(&self, Parameters(p): Parameters<ActivateParams>) -> CallToolResult {
        self.act(|session, policy| session.activate(policy, p.window_id))
    }

    #[tool(
        description = "Look at a window: its element tree and/or a screenshot. Each tree line is \
                       `e<N> role \"label\" =\"value\" [x,y WxH]`; coordinates are pixels of the \
                       window screenshot. Refs (e<N>) are valid until the next observe that \
                       returns a tree or the next wait_for, whatever its outcome. To keep the tree short, print only part of it with `find` \
                       (text in label or value), `role`, `interactive` (controls only) and \
                       `max_depth`; matches are listed without their parents, and refs of hidden \
                       elements still work."
    )]
    fn observe(&self, Parameters(params): Parameters<ObserveParams>) -> CallToolResult {
        let request = ObserveRequest {
            window_id: params.window_id,
            screenshot: match params.screenshot.unwrap_or(ScreenshotMode::Window) {
                ScreenshotMode::None => Shot::None,
                ScreenshotMode::Window => Shot::Window,
                ScreenshotMode::Region => Shot::Region,
            },
            tree: params.tree.unwrap_or(true),
            reference: params.reference,
            max_edge: params.max_edge.unwrap_or(DEFAULT_MAX_EDGE).max(1),
            filter: Filter::new(params.find, params.role, params.interactive.unwrap_or(false), params.max_depth),
        };
        let observation = match self.policy().and_then(|p| self.session.observe(&p, request).map_err(|e| e.to_string())) {
            Ok(observation) => observation,
            Err(message) => return failure(message),
        };
        let mut content = vec![ContentBlock::text(observation.text)];
        if let Some(png) = observation.png {
            content.push(ContentBlock::image(BASE64.encode(png), "image/png"));
        }
        CallToolResult::success(content)
    }

    #[tool(
        description = "Wait until an element matching `find` and/or `role` appears in a window, \
                       or with `gone` until none is left, instead of sleeping. Returns the \
                       matching lines as observe would, with refs usable at once, or an error \
                       after timeout_ms. Either way, refs from earlier observes stop working."
    )]
    async fn wait_for(
        &self,
        Parameters(p): Parameters<WaitForParams>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let policy = match self.policy() {
            Ok(policy) => policy,
            Err(message) => return failure(message),
        };
        let request = WaitRequest {
            window_id: p.window_id,
            filter: Filter::new(p.find, p.role, false, None),
            gone: p.gone.unwrap_or(false),
            max_edge: DEFAULT_MAX_EDGE,
        };
        let timeout = Duration::from_millis(p.timeout_ms.unwrap_or(DEFAULT_WAIT_MS).min(MAX_WAIT_MS));
        let session = self.session.clone();
        // rmcp does not drop a handler whose request the client cancels; it
        // only cancels this token, so the wait itself has to check it.
        let cancel = context.ct;
        // rmcp calls tool handlers on the runtime's worker threads; a wait of
        // up to 30 s there would hold a worker other requests need.
        let waited = tokio::task::spawn_blocking(move || {
            session.wait_for(&policy, request, timeout, WAIT_POLL, &|| cancel.is_cancelled())
        })
        .await;
        match waited {
            Ok(Ok(found)) => text(found),
            Ok(Err(error)) => failure(error.to_string()),
            Err(error) => failure(format!("wait_for stopped unexpectedly: {error}")),
        }
    }

    #[tool(
        description = "Click an element (by ref) or a screenshot pixel in a window. Tries \
                       accessibility first, which does not move the user's pointer; the result \
                       says how the click was delivered. Observe again afterwards to see the effect."
    )]
    fn click(&self, Parameters(p): Parameters<ClickParams>) -> CallToolResult {
        let at = match target(p.reference, p.x, p.y) {
            Ok(Some(at)) => at,
            Ok(None) => return failure("click needs `ref`, or `x` and `y`".to_string()),
            Err(message) => return failure(message),
        };
        let button = match p.button.unwrap_or(MouseButton::Left) {
            MouseButton::Left => Button::Left,
            MouseButton::Right => Button::Right,
        };
        self.act(|session, policy| {
            session.click(policy, p.window_id, &at, button, p.double.unwrap_or(false), mode(p.mode))
        })
    }

    #[tool(
        description = "Type text into an element (by ref) or into whatever has focus in a window. \
                       Tries accessibility first, which does not use the keyboard; the result says \
                       how the text was delivered. For Enter, shortcuts and other keys use press_key."
    )]
    fn type_text(&self, Parameters(p): Parameters<TypeTextParams>) -> CallToolResult {
        self.act(|session, policy| {
            session.type_text(
                policy,
                p.window_id,
                p.reference.as_deref(),
                &p.text,
                p.replace.unwrap_or(false),
                mode(p.mode),
            )
        })
    }

    #[tool(
        description = "Set a checkbox, switch or radio button on or off, a slider or stepper to a \
                       number, or a pop-up button to one of its options, through accessibility \
                       only: the pointer, keyboard and focus are not used. Does nothing if the \
                       control already has that value. The result says what the control holds \
                       afterwards; a slider may snap to a step."
    )]
    fn set_value(&self, Parameters(p): Parameters<SetValueParams>) -> CallToolResult {
        let value = match p.value {
            ControlValue::Text(text) => text,
            ControlValue::Number(n) => n.to_string(),
        };
        self.act(|session, policy| session.set_value(policy, p.window_id, &p.reference, &value))
    }

    #[tool(
        description = "List or press items of the menu bar of a window's app through \
                       accessibility only: no menu opens on screen, and the pointer, keyboard and \
                       focus are not used. Without path, lists the menus. A path ending at a menu \
                       or submenu lists its items, each with its shortcut and the words checked, \
                       disabled, submenu that apply; a path ending at an item presses it. A press \
                       acts on the app's main window, so window_id must be that window (call \
                       activate first). A title containing \">\" cannot be named. The Apple and \
                       Services menus are never available."
    )]
    fn menu(&self, Parameters(p): Parameters<MenuParams>) -> CallToolResult {
        self.act(|session, policy| session.menu(policy, p.window_id, p.path.as_deref()))
    }

    #[tool(
        description = "Press a key or shortcut in a window's app, e.g. \"enter\" or \"cmd+s\". \
                       The app is brought to the front. The keys reach only that app, never \
                       system-wide shortcuts."
    )]
    fn press_key(&self, Parameters(p): Parameters<PressKeyParams>) -> CallToolResult {
        self.act(|session, policy| session.press_key(policy, p.window_id, &p.keys))
    }

    #[tool(
        description = "Scroll in a window by lines, over an element, a screenshot pixel, or the \
                       middle of the window. Uses the real pointer and brings the app to the front."
    )]
    fn scroll(&self, Parameters(p): Parameters<ScrollParams>) -> CallToolResult {
        let at = match target(p.reference, p.x, p.y) {
            Ok(at) => at,
            Err(message) => return failure(message),
        };
        let (dx, dy) = (p.dx.unwrap_or(0), p.dy.unwrap_or(0));
        if (dx, dy) == (0, 0) {
            return failure("scroll needs a non-zero `dy` or `dx`".to_string());
        }
        self.act(|session, policy| session.scroll(policy, p.window_id, at.as_ref(), dx, dy))
    }

    #[tool(
        description = "Drag with the real pointer inside one window: press the left button at \
                       `from` (from_ref, or from_x and from_y), move to `to` (to_ref, or to_x and \
                       to_y) and release, e.g. to select text, move a slider or divider, or \
                       reorder items. Brings the app to the front; takes about 1.1 s. If the \
                       window stops being on top along the way, or the user moves the mouse, the \
                       drag is cancelled and the result says where the button was released. \
                       Observe again afterwards to see the effect."
    )]
    fn drag(&self, Parameters(p): Parameters<DragParams>) -> CallToolResult {
        self.act(|session, policy| {
            let from = drag_end("from", p.from_ref, p.from_x, p.from_y)?;
            let to = drag_end("to", p.to_ref, p.to_x, p.to_y)?;
            session.drag(policy, p.window_id, &from, &to)
        })
    }

    /// Runs an action under the current policy and reports how it went.
    fn act(
        &self,
        action: impl FnOnce(&Session, &AccessPolicy) -> Result<String, crate::domain::session::Error>,
    ) -> CallToolResult {
        match self.policy().and_then(|p| action(&self.session, &p).map_err(|e| e.to_string())) {
            Ok(done) => text(done),
            Err(message) => failure(message),
        }
    }

    /// Re-read on every call so `look-at-this allow` applies without a restart.
    fn policy(&self) -> Result<AccessPolicy, String> {
        let config = config::load(&self.config_path).map_err(|e| format!("config is unreadable: {e}"))?;
        Ok(AccessPolicy::new(&config.allowed_apps))
    }
}

/// `id app (bundle) "title" WxH` and the state words that apply, in a fixed order.
fn window_line(listed: &ListedWindow) -> String {
    let w = &listed.window;
    let mut line = format!(
        "{} {} ({}) \"{}\" {}x{}",
        w.id,
        one_line(&w.app_name),
        w.bundle_id.as_deref().unwrap_or_default(),
        one_line(&w.title),
        w.frame.width.round(),
        w.frame.height.round()
    );
    for (applies, word) in [(listed.frontmost, "frontmost"), (listed.thumbnail, "thumbnail"), (!w.on_screen, "off-screen")] {
        if applies {
            let _ = write!(line, " {word}");
        }
    }
    line.push('\n');
    line
}

fn text(message: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(message)])
}

fn failure(message: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

#[tool_handler(
    name = "look-at-this",
    instructions = "See and control the windows of macOS apps the user has allowed. \
                    Only the user can change the allowed list. Start with doctor, then \
                    list_windows, then observe; a window listed as thumbnail or off-screen needs \
                    activate first. Act with click, type_text, set_value, menu, press_key, scroll, \
                    drag, and observe again to check the result; to wait for the UI to change, use \
                    wait_for rather than sleeping."
)]
impl ServerHandler for Server {}

pub async fn serve(server: Server) -> Result<(), Box<dyn Error + Send + Sync>> {
    server.serve(stdio()).await?.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::{Rect, WindowInfo};

    fn listed(frontmost: bool, thumbnail: bool, on_screen: bool) -> ListedWindow {
        let window = WindowInfo {
            id: 7,
            pid: 70,
            bundle_id: Some("com.apple.TextEdit".to_string()),
            app_name: "TextEdit".to_string(),
            title: "Notes.txt".to_string(),
            frame: Rect { x: 0.0, y: 0.0, width: 99.4, height: 116.0 },
            on_screen,
        };
        ListedWindow { window, frontmost, thumbnail }
    }

    #[test]
    fn window_line_ends_with_the_state_words_that_apply() {
        assert_eq!(
            window_line(&listed(false, false, true)),
            "7 TextEdit (com.apple.TextEdit) \"Notes.txt\" 99x116\n"
        );
        assert_eq!(
            window_line(&listed(true, true, false)),
            "7 TextEdit (com.apple.TextEdit) \"Notes.txt\" 99x116 frontmost thumbnail off-screen\n"
        );
    }
}
