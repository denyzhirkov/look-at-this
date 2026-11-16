//! The agent's cursor, drawn in a click-through window above everything.
//!
//! AppKit runs on the main thread (see `main.rs`); `point_at` only queues the
//! work there, so it never waits for drawing or for the app to finish launching.

use std::cell::{Cell, OnceCell};
use std::ptr::{self, NonNull};
use std::time::Duration;

use block2::{DynBlock, RcBlock};
use dispatch2::{DispatchQueue, DispatchTime};
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAnimatablePropertyContainer, NSAnimationContext, NSBackingStoreType, NSColor, NSScreenSaverWindowLevel,
    NSWindow, NSWindowAnimationBehavior, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGColor, CGDisplayBounds, CGMainDisplayID, CGMutablePath};
use objc2_foundation::{NSNumber, NSString};
use objc2_quartz_core::{
    CABasicAnimation, CACurrentMediaTime, CALayer, CAMediaTiming, CAMediaTimingFunction, CAShapeLayer,
    kCAMediaTimingFunctionEaseOut,
};

use crate::ports::{Overlay, Point};

/// Side of the square window, in points; the action point is its centre.
const SIZE: f64 = 64.0;
const RING_RADIUS: f64 = 22.0;
/// The arrowhead, tip first, relative to the tip (y up).
const ARROW: [(f64, f64); 4] = [(0.0, 0.0), (0.0, -20.0), (5.5, -15.5), (14.5, -14.5)];
const GLIDE_SECS: f64 = 0.2;
const PULSE_SECS: f64 = 0.45;
const FADE_SECS: f64 = 0.4;
/// How long the cursor stays after the last action before it fades.
const IDLE: Duration = Duration::from_secs(3);

pub struct MacOverlay;

impl Overlay for MacOverlay {
    fn point_at(&self, point: Point) {
        DispatchQueue::main().exec_async(move || with_cursor(|cursor| cursor.show(point)));
    }
}

thread_local! {
    /// Lives on the main thread: `with_cursor` reaches it only there.
    static CURSOR: OnceCell<Option<Cursor>> = const { OnceCell::new() };
}

/// Runs `act` on the cursor, creating it on first use. Does nothing off the
/// main thread or if AppKit could not make the window.
fn with_cursor(act: impl FnOnce(&Cursor)) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    CURSOR.with(|cursor| {
        if let Some(cursor) = cursor.get_or_init(|| Cursor::new(mtm)) {
            act(cursor);
        }
    });
}

struct Cursor {
    window: Retained<NSWindow>,
    ring: Retained<CALayer>,
    /// Counts `show` calls, so a fade scheduled by an earlier one can tell
    /// that the cursor has moved since.
    shown: Cell<u64>,
}

impl Cursor {
    fn new(mtm: MainThreadMarker) -> Option<Self> {
        let bounds = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(SIZE, SIZE));
        // SAFETY: a borderless, buffered window with a valid content rect.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                bounds,
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: the window is owned by `Cursor` and never closed.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setOpaque(false);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setHasShadow(false);
        // The window server then skips the window in hit tests: clicks go to
        // what is underneath, and the topmost check in input.rs never sees it.
        // A borderless window cannot become key or main.
        window.setIgnoresMouseEvents(true);
        // Above app windows, the Dock and menus. Not on an app window level, so
        // the window list (screen.rs) and Stage Manager leave it alone.
        window.setLevel(NSScreenSaverWindowLevel);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle
                | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        window.setAnimationBehavior(NSWindowAnimationBehavior::None);

        let orange = CGColor::new_srgb(1.0, 0.42, 0.0, 1.0);
        let centre = SIZE / 2.0;
        let root = CALayer::new();
        let ring = CALayer::new();
        ring.setBounds(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(2.0 * RING_RADIUS, 2.0 * RING_RADIUS)));
        ring.setPosition(CGPoint::new(centre, centre));
        ring.setCornerRadius(RING_RADIUS);
        ring.setBorderWidth(3.0);
        ring.setBorderColor(Some(&orange));
        ring.setOpacity(0.0);
        root.addSublayer(&ring);

        let arrow = CAShapeLayer::new();
        arrow.setFrame(bounds);
        arrow.setContentsScale(window.backingScaleFactor());
        let points = ARROW.map(|(x, y)| CGPoint::new(centre + x, centre + y));
        let path = CGMutablePath::new();
        // SAFETY: no transform; `points` is valid for `points.len()` points.
        unsafe { CGMutablePath::add_lines(Some(&path), ptr::null(), points.as_ptr(), points.len()) };
        CGMutablePath::close_subpath(Some(&path));
        arrow.setPath(Some(&path));
        arrow.setFillColor(Some(&orange));
        arrow.setStrokeColor(Some(&CGColor::new_srgb(1.0, 1.0, 1.0, 1.0)));
        arrow.setLineWidth(1.5);
        arrow.setShadowColor(Some(&CGColor::new_srgb(0.0, 0.0, 0.0, 1.0)));
        arrow.setShadowOpacity(0.4);
        arrow.setShadowRadius(1.5);
        arrow.setShadowOffset(CGSize::new(0.0, -1.0));
        root.addSublayer(&arrow);

        let view = window.contentView()?;
        // A layer-hosting view: it shows `root` and draws nothing itself.
        view.setLayer(Some(&root));
        view.setWantsLayer(true);
        Some(Self { window, ring, shown: Cell::new(0) })
    }

    fn show(&self, point: Point) {
        let shown = self.shown.get() + 1;
        self.shown.set(shown);
        let frame = CGRect::new(window_origin(point, main_display_height()), CGSize::new(SIZE, SIZE));
        let window = &self.window;
        // Glide from where it is, unless it has disappeared: then it appears at once.
        let glide = if window.isVisible() {
            animate(
                GLIDE_SECS,
                || {
                    let animator = window.animator();
                    animator.setFrame_display(frame, false);
                    // Cancels a fade in progress.
                    animator.setAlphaValue(1.0);
                },
                None,
            );
            GLIDE_SECS
        } else {
            window.setFrame_display(frame, false);
            window.setAlphaValue(1.0);
            0.0
        };
        // Does not activate this process, unlike orderFront.
        window.orderFrontRegardless();
        self.pulse(glide);
        if let Ok(when) = DispatchTime::try_from(Duration::from_secs_f64(glide) + IDLE) {
            let _ = DispatchQueue::main().after(when, move || with_cursor(|cursor| cursor.fade(shown)));
        }
    }

    /// A ring that grows and fades around the point, starting after `delay` seconds.
    fn pulse(&self, delay: f64) {
        let begin = CACurrentMediaTime() + delay;
        // SAFETY: a QuartzCore constant, valid for the life of the process.
        let ease_out = CAMediaTimingFunction::functionWithName(unsafe { kCAMediaTimingFunctionEaseOut });
        for (key_path, from, to) in [("transform.scale", 0.3, 1.0), ("opacity", 0.9, 0.0)] {
            let key_path = NSString::from_str(key_path);
            let animation = CABasicAnimation::animationWithKeyPath(Some(&key_path));
            // SAFETY: both properties are animated with numbers.
            unsafe {
                animation.setFromValue(Some(&NSNumber::new_f64(from)));
                animation.setToValue(Some(&NSNumber::new_f64(to)));
            }
            animation.setBeginTime(begin);
            animation.setDuration(PULSE_SECS);
            animation.setTimingFunction(Some(&ease_out));
            // Same key: replaces the pulse of an earlier action.
            self.ring.addAnimation_forKey(&animation, Some(&key_path));
        }
    }

    /// Fades the cursor out, unless it was shown again after call `shown`.
    fn fade(&self, shown: u64) {
        if self.shown.get() != shown {
            return;
        }
        let window = &self.window;
        let done = RcBlock::new(move || {
            with_cursor(|cursor| {
                if cursor.shown.get() == shown {
                    cursor.window.orderOut(None);
                }
            })
        });
        animate(FADE_SECS, || window.animator().setAlphaValue(0.0), Some(&done));
    }
}

/// Runs `changes` to the animator proxies as one eased animation.
fn animate(seconds: f64, changes: impl Fn(), done: Option<&DynBlock<dyn Fn()>>) {
    let changes = RcBlock::new(move |context: NonNull<NSAnimationContext>| {
        // SAFETY: AppKit passes the current context, valid for this call.
        let context = unsafe { context.as_ref() };
        context.setDuration(seconds);
        // SAFETY: a QuartzCore constant, valid for the life of the process.
        let ease_out = CAMediaTimingFunction::functionWithName(unsafe { kCAMediaTimingFunctionEaseOut });
        context.setTimingFunction(Some(&ease_out));
        changes();
    });
    NSAnimationContext::runAnimationGroup_completionHandler(&changes, done);
}

/// Height of the main display: the one whose top-left is the origin of the
/// ports' coordinates and whose bottom-left is the origin of AppKit's.
fn main_display_height() -> f64 {
    CGDisplayBounds(CGMainDisplayID()).size.height
}

/// Where AppKit must put the bottom-left corner of the cursor window so that
/// its centre is at `point`. Both spaces share the x axis; y points down from
/// the main display's top in the ports and up from its bottom in AppKit, which
/// holds for every display, including ones above or left of the main one.
fn window_origin(point: Point, main_height: f64) -> CGPoint {
    CGPoint::new(point.x - SIZE / 2.0, main_height - point.y - SIZE / 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_is_centred_on_the_point_with_y_flipped_at_the_main_display() {
        let half = SIZE / 2.0;
        // Top-left of the main display.
        assert_eq!(window_origin(Point { x: 0.0, y: 0.0 }, 900.0), CGPoint::new(-half, 900.0 - half));
        assert_eq!(window_origin(Point { x: 300.0, y: 200.0 }, 900.0), CGPoint::new(300.0 - half, 700.0 - half));
        // A display above and to the left of the main one.
        assert_eq!(window_origin(Point { x: -500.0, y: -100.0 }, 900.0), CGPoint::new(-500.0 - half, 1000.0 - half));
    }
}
