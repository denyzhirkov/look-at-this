//! Window list and window screenshots via ScreenCaptureKit.

use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::{AnyThread, msg_send};
use objc2_core_foundation::{CFMutableData, CFString, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGImage, CGMainDisplayID};
use objc2_foundation::{NSArray, NSError, NSString};
use objc2_image_io::CGImageDestination;
use objc2_screen_capture_kit::{
    SCCaptureResolutionType, SCContentFilter, SCScreenshotManager, SCShareableContent,
    SCStreamConfiguration, SCStreamErrorCode, SCWindow,
};

use crate::ports::{PortResult, Rect, ScreenCapture, WindowInfo, WindowSource};

/// ScreenCaptureKit answers within milliseconds; longer means it is stuck.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Smaller windows are invisible helpers (e.g. 1x1 px utility windows).
const MIN_WINDOW_EDGE: f64 = 20.0;

/// Window levels an app's own windows live on: normal (0), floating panels
/// (3) and modal dialogs such as alerts (8). Menus, the Dock and other system
/// layers are above these.
const APP_WINDOW_LAYERS: [isize; 3] = [0, 3, 8];

pub struct MacScreen;

impl WindowSource for MacScreen {
    fn windows(&self) -> PortResult<Vec<WindowInfo>> {
        with_content(|content, done| {
            let _ = done.send(Ok(windows(content).iter().filter_map(|w| window_info(w)).collect()));
        })
    }
}

impl ScreenCapture for MacScreen {
    fn capture(&self, window_id: u32, pid: i32, region: Rect, width: u32, height: u32) -> PortResult<Vec<u8>> {
        with_content(move |content, done| {
            let windows = windows(content);
            let Some(window) = windows.iter().find(|w| unsafe { w.windowID() } == window_id) else {
                let _ = done.send(Err(format!(
                    "window {window_id} is no longer on screen; call list_windows"
                )));
                return;
            };
            // Window ids are reused: the caller's access check was for `pid`.
            let owner = unsafe { window.owningApplication() }.map(|app| unsafe { app.processID() });
            if owner != Some(pid) {
                let _ = done.send(Err(format!(
                    "window {window_id} now belongs to another app; call list_windows"
                )));
                return;
            }
            let filter: Option<Retained<SCContentFilter>> = unsafe {
                msg_send![SCContentFilter::alloc(), initWithDesktopIndependentWindow: &**window]
            };
            let Some(filter) = filter else {
                let _ = done.send(Err(format!("ScreenCaptureKit cannot capture window {window_id}")));
                return;
            };
            let config = unsafe { SCStreamConfiguration::new() };
            unsafe {
                config.setWidth(width as usize);
                config.setHeight(height as usize);
                config.setSourceRect(CGRect::new(
                    CGPoint::new(region.x, region.y),
                    CGSize::new(region.width, region.height),
                ));
                // Without this, a region from a 1x display is not scaled up:
                // it lands unscaled in the top-left corner of the image.
                config.setScalesToFit(true);
                config.setShowsCursor(false);
                config.setIgnoreShadowsSingleWindow(true);
                // Sample the Retina backing store so magnified regions stay sharp.
                config.setCaptureResolution(SCCaptureResolutionType::Best);
            }
            let handler = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
                // SAFETY: SCK passes a valid image or null, borrowed for the call.
                let result = match unsafe { image.as_ref() } {
                    Some(image) => encode_png(image, width, height),
                    None => Err(sck_error("capturing the window", error)),
                };
                let _ = done.send(result);
            });
            unsafe {
                SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
                    &filter,
                    &config,
                    Some(&handler),
                );
            }
        })
    }
}

/// Fetches the on-screen windows and runs `then` on ScreenCaptureKit's
/// callback queue; `then` reports through the sender, possibly from a
/// further callback. Blocks the calling thread until a result or timeout.
fn with_content<T: Send + 'static>(
    then: impl Fn(&SCShareableContent, Sender<PortResult<T>>) + Send + 'static,
) -> PortResult<T> {
    // Without AppKit nothing connects this process to the window server, and
    // SCScreenshotManager then aborts the process (CGS_REQUIRE_INIT). Any
    // CoreGraphics display call makes that connection.
    CGMainDisplayID();
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        // SAFETY: SCK passes valid content or null, borrowed for the call.
        match unsafe { content.as_ref() } {
            Some(content) => then(content, tx.clone()),
            None => {
                let _ = tx.send(Err(sck_error("listing windows", error)));
            }
        }
    });
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, true, &handler,
        );
    }
    rx.recv_timeout(TIMEOUT).unwrap_or_else(|_| {
        Err(format!(
            "ScreenCaptureKit did not answer within {} s. Either a Screen Recording permission \
             prompt is waiting for the user, or ScreenCaptureKit dropped the request; ask the \
             user to answer any prompt, then try again",
            TIMEOUT.as_secs()
        ))
    })
}

// Here, in `window_info` and for the content filter in `capture`, msg_send!
// with an Option result replaces bindings that declare the result non-null
// and would panic on nil inside SCK's callback queue, taking the server down.

fn windows(content: &SCShareableContent) -> Vec<Retained<SCWindow>> {
    let windows: Option<Retained<NSArray<SCWindow>>> = unsafe { msg_send![content, windows] };
    windows.map(|w| w.to_vec()).unwrap_or_default()
}

/// `None` for anything that is not an ordinary visible app window.
fn window_info(window: &SCWindow) -> Option<WindowInfo> {
    let app = unsafe { window.owningApplication() }?;
    // SCWindow.frame is in global display points, origin at the top-left of
    // the main display: the same space as WindowInfo.frame.
    let frame = unsafe { window.frame() };
    let ordinary = unsafe { APP_WINDOW_LAYERS.contains(&window.windowLayer()) && window.isOnScreen() };
    if !ordinary || frame.size.width < MIN_WINDOW_EDGE || frame.size.height < MIN_WINDOW_EDGE {
        return None;
    }
    let bundle_id: Option<Retained<NSString>> = unsafe { msg_send![&*app, bundleIdentifier] };
    let app_name: Option<Retained<NSString>> = unsafe { msg_send![&*app, applicationName] };
    Some(WindowInfo {
        id: unsafe { window.windowID() },
        pid: unsafe { app.processID() },
        bundle_id: bundle_id.map(|id| id.to_string()).filter(|id| !id.is_empty()),
        app_name: app_name.map(|name| name.to_string()).unwrap_or_default(),
        title: unsafe { window.title() }.map(|t| t.to_string()).unwrap_or_default(),
        frame: Rect {
            x: frame.origin.x,
            y: frame.origin.y,
            width: frame.size.width,
            height: frame.size.height,
        },
    })
}

fn encode_png(image: &CGImage, width: u32, height: u32) -> PortResult<Vec<u8>> {
    let (got_width, got_height) = (CGImage::width(Some(image)), CGImage::height(Some(image)));
    if (got_width, got_height) != (width as usize, height as usize) {
        return Err(format!(
            "screenshot came back {got_width}x{got_height} px instead of {width}x{height}; try again"
        ));
    }
    let data = CFMutableData::new(None, 0).ok_or("could not allocate memory for the PNG")?;
    let png_type = CFString::from_static_str("public.png");
    let destination = unsafe { CGImageDestination::with_data(&data, &png_type, 1, None) }
        .ok_or("ImageIO cannot create PNG images")?;
    let finished = unsafe {
        destination.add_image(image, None);
        destination.finalize()
    };
    if !finished {
        return Err("encoding the screenshot as PNG failed".to_string());
    }
    Ok(data.to_vec())
}

fn sck_error(action: &str, error: *mut NSError) -> String {
    // SAFETY: SCK passes a valid error or null, borrowed for the call.
    let Some(error) = (unsafe { error.as_ref() }) else {
        return format!("{action} failed without an error from ScreenCaptureKit");
    };
    if error.code() == SCStreamErrorCode::UserDeclined.0 {
        return "Screen Recording permission is missing. The user must enable it for the app \
                running this server in System Settings > Privacy & Security > Screen & System \
                Audio Recording, then restart that app."
            .to_string();
    }
    let description = error.localizedDescription();
    if error.code() == SCStreamErrorCode::InternalError.0 {
        return format!(
            "{action} failed: {description}. If Stage Manager is on and the app is in the \
             background, its window is only a thumbnail: ask the user to bring the app to the front."
        );
    }
    format!("{action} failed: {description}")
}
