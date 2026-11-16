//! MCP server: thin handlers over the domain.

use std::error::Error;
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::transport::stdio;
use rmcp::{ErrorData, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::config;
use crate::domain::doctor;
use crate::domain::policy::AccessPolicy;
use crate::domain::actions::{At, Mode};
use crate::domain::session::{ObserveRequest, Session, Shot};
use crate::domain::snapshot::one_line;
use crate::ports::{Button, Permissions};

const DEFAULT_MAX_EDGE: u32 = 1280;

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
        let report = doctor::report(self.permissions.as_ref(), &self.config_path, config);
        Ok(CallToolResult::success(vec![ContentBlock::json(report)?]))
    }

    #[tool(description = "List the open windows of allowed apps, with the ids that observe takes.")]
    fn list_windows(&self) -> CallToolResult {
        let windows = match self.policy().and_then(|p| self.session.list_windows(&p).map_err(|e| e.to_string())) {
            Ok(windows) => windows,
            Err(message) => return failure(message),
        };
        if windows.is_empty() {
            return text("No windows of allowed apps are open. Call doctor to see which apps are allowed.");
        }
        let mut out = String::new();
        for w in windows {
            let _ = writeln!(
                out,
                "{} {} ({}) \"{}\" {}x{}",
                w.id,
                one_line(&w.app_name),
                w.bundle_id.unwrap_or_default(),
                one_line(&w.title),
                w.frame.width.round(),
                w.frame.height.round()
            );
        }
        text(out)
    }

    #[tool(
        description = "Look at a window: its element tree and/or a screenshot. Each tree line is \
                       `e<N> role \"label\" =\"value\" [x,y WxH]`; coordinates are pixels of the \
                       window screenshot. Refs (e<N>) are valid until the next observe that \
                       returns a tree."
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
                    list_windows, then observe; act with click, type_text, press_key, scroll, \
                    and observe again to check the result."
)]
impl ServerHandler for Server {}

pub async fn serve(server: Server) -> Result<(), Box<dyn Error + Send + Sync>> {
    server.serve(stdio()).await?.waiting().await?;
    Ok(())
}
