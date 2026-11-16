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
use crate::domain::session::{ObserveRequest, Session, Shot};
use crate::domain::snapshot::one_line;
use crate::ports::Permissions;

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
                    list_windows, then observe."
)]
impl ServerHandler for Server {}

pub async fn serve(server: Server) -> Result<(), Box<dyn Error + Send + Sync>> {
    server.serve(stdio()).await?.waiting().await?;
    Ok(())
}
