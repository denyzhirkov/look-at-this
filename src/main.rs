mod adapters;
mod config;
mod domain;
mod ports;

use std::error::Error;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use adapters::macos::{MacInput, MacPermissions, MacScreen, MacUiTree};
use adapters::mcp;
use domain::policy::{Access, AccessPolicy};
use domain::session::{Ports, Session};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

const USAGE: &str = "usage:
  look-at-this                    run the MCP server on stdio
  look-at-this allow <bundle-id>  let agents see and control an app";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        [] => serve(),
        ["allow", bundle_id] => allow(bundle_id),
        _ => Err(USAGE.into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn serve() -> Result<()> {
    let screen = Arc::new(MacScreen);
    // Refs start at a different thousand each run (see Session::new).
    let run = SystemTime::now().duration_since(UNIX_EPOCH)?.subsec_nanos() as u64 % 900;
    let ui = Arc::new(MacUiTree::default());
    let ports = Ports {
        windows: screen.clone(),
        capture: screen,
        tree: ui.clone(),
        actions: ui,
        // Here, on the main thread, before the runtime starts (see MacInput::new).
        input: Arc::new(MacInput::new().map_err(|e| format!("cannot start: {e}"))?),
    };
    let session = Session::new(ports, (run + 1) * 1000);
    let server = mcp::Server::new(Arc::new(MacPermissions), session, config::default_path()?);
    tokio::runtime::Runtime::new()?.block_on(mcp::serve(server))
}

fn allow(bundle_id: &str) -> Result<()> {
    let path = config::default_path()?;
    let mut config = config::load(&path)?;
    match AccessPolicy::new(&config.allowed_apps).check(bundle_id) {
        Access::Allowed => println!("{bundle_id} is already allowed"),
        Access::Forbidden => {
            return Err(format!(
                "{bundle_id} can never be controlled: it could be used to bypass agent safeguards"
            )
            .into());
        }
        Access::NotAllowed => {
            config.allowed_apps.push(bundle_id.trim().to_string());
            config::save(&path, &config)?;
            println!("{bundle_id} allowed ({})", path.display());
        }
    }
    Ok(())
}
