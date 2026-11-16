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

use adapters::macos::{
    MacConsent, MacInput, MacOverlay, MacPause, MacPermissions, MacScreen, MacUiTree,
};
use adapters::mcp;
use domain::allow::{self, Outcome};
use domain::session::{Ports, Session};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

const USAGE: &str = "usage:
  look-at-this                    run the MCP server on stdio
  look-at-this allow <bundle-id>  let agents see and control an app (asks you to confirm)";

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
    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon or menu bar; never the active app, so it never takes focus.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let config_path = config::default_path()?;
    // No server runs without the user's emergency stop.
    let pause = MacPause::install(mtm, config_path.with_file_name("paused")).map_err(|e| format!("cannot start: {e}"))?;
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
        pause: Arc::new(pause),
    };
    let session = Session::new(ports, (run + 1) * 1000);
    let server = mcp::Server::new(Arc::new(MacPermissions), session, config_path);
    let runtime = tokio::runtime::Runtime::new()?;
    thread::Builder::new().name("mcp".into()).spawn(move || {
        let result = runtime.block_on(mcp::serve(server));
        // On the main thread, where AppKit expects exit handlers to run.
        DispatchQueue::main().exec_async(move || process::exit(report(result).into()));
    })?;
    app.run();
    Err("the AppKit event loop stopped unexpectedly".into())
}

fn allow(bundle_id: &str) -> Result<()> {
    let bundle_id = bundle_id.trim();
    let path = config::default_path()?;
    let mut config = config::load(&path)?;
    match allow::allow(&mut config, bundle_id, &MacConsent)? {
        Outcome::AlreadyAllowed => println!("{bundle_id} is already allowed"),
        Outcome::Declined => return Err(format!("{bundle_id} was not allowed: the confirmation dialog was declined or not answered").into()),
        Outcome::Added => {
            config::save(&path, &config)?;
            println!("{bundle_id} allowed ({})", path.display());
        }
    }
    Ok(())
}
