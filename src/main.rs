mod adapters;
mod config;
mod domain;
mod ports;

use std::error::Error;
use std::process::{self, ExitCode};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use dispatch2::DispatchQueue;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

use adapters::macos::{MacInput, MacOverlay, MacPermissions, MacScreen, MacUiTree};
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
    ExitCode::from(report(result))
}

/// Prints the error, if any, and returns the process exit status.
fn report(result: Result<()>) -> u8 {
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            1
        }
    }
}

/// AppKit, which draws the agent's cursor, owns the main thread, so the MCP
/// server runs on the runtime's threads and the process exits when it ends.
fn serve() -> Result<()> {
    let mtm = MainThreadMarker::new().ok_or("the server must start on the main thread")?;
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
        overlay: Arc::new(MacOverlay),
    };
    let session = Session::new(ports, (run + 1) * 1000);
    let server = mcp::Server::new(Arc::new(MacPermissions), session, config::default_path()?);
    let runtime = tokio::runtime::Runtime::new()?;
    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon or menu bar; never the active app, so it never takes focus.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    thread::Builder::new().name("mcp".into()).spawn(move || {
        let result = runtime.block_on(mcp::serve(server));
        // On the main thread, where AppKit expects exit handlers to run.
        DispatchQueue::main().exec_async(move || process::exit(report(result).into()));
    })?;
    app.run();
    Err("the AppKit event loop stopped unexpectedly".into())
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
