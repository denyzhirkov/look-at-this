//! Mapping between a window and the image the agent sees.

use crate::ports::Rect;

/// A part of a window shown to the agent at some scale. Every coordinate the
/// agent receives is in this image's pixel space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    /// Shown area, in points relative to the window's top-left.
    pub region: Rect,
    /// Image pixels per point.
    pub scale: f64,
}

impl Viewport {
    /// Fits `region` so its longest side is at most `max_edge` pixels, never
    /// magnifying beyond `max_scale`.
    pub fn fit(region: Rect, max_scale: f64, max_edge: u32) -> Self {
        let longest = region.width.max(region.height).max(1.0);
        Self { region, scale: max_scale.min(f64::from(max_edge) / longest) }
    }

    pub fn image_size(&self) -> (u32, u32) {
        let px = |points: f64| ((points * self.scale).round() as u32).max(1);
        (px(self.region.width), px(self.region.height))
    }

    /// Converts an image pixel to a window-relative point, if it is inside
    /// the image.
    pub fn window_point(self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (width, height) = self.image_size();
        let inside = (0.0..f64::from(width)).contains(&x) && (0.0..f64::from(height)).contains(&y);
        inside.then(|| (self.region.x + x / self.scale, self.region.y + y / self.scale))
    }

    /// Converts a window-relative rectangle to image pixels.
    pub fn to_image(self, rect: Rect) -> Rect {
        Rect {
            x: (rect.x - self.region.x) * self.scale,
            y: (rect.y - self.region.y) * self.scale,
            width: rect.width * self.scale,
            height: rect.height * self.scale,
        }
    }
}

/// The part of `rect` inside a `width` x `height` window, if any.
pub fn clamp_to_window(rect: Rect, width: f64, height: f64) -> Option<Rect> {
    let left = rect.x.max(0.0);
    let top = rect.y.max(0.0);
    let right = (rect.x + rect.width).min(width);
    let bottom = (rect.y + rect.height).min(height);
    (right > left && bottom > top)
        .then_some(Rect { x: left, y: top, width: right - left, height: bottom - top })
}

/// With Stage Manager on, a background app's window is listed at the size of
/// its side-strip thumbnail while accessibility still reports the real window.
/// Such a window cannot be captured, and tree coordinates would not match it.
pub fn is_thumbnail(listed: Rect, actual: Rect) -> bool {
    const SHRUNK: f64 = 0.8;
    listed.width < actual.width * SHRUNK || listed.height < actual.height * SHRUNK
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect { x, y, width, height }
    }

    #[test]
    fn large_window_is_downscaled_to_max_edge() {
        let v = Viewport::fit(rect(0.0, 0.0, 2000.0, 1000.0), 1.0, 1000);
        assert_eq!(v.scale, 0.5);
        assert_eq!(v.image_size(), (1000, 500));
        assert_eq!(v.to_image(rect(200.0, 100.0, 40.0, 20.0)), rect(100.0, 50.0, 20.0, 10.0));
    }

    #[test]
    fn image_pixel_maps_back_to_the_window() {
        let v = Viewport::fit(rect(0.0, 0.0, 2000.0, 1000.0), 1.0, 1000);
        assert_eq!(v.window_point(100.0, 50.0), Some((200.0, 100.0)));
        assert_eq!(v.window_point(1000.0, 50.0), None, "one past the right edge");
        assert_eq!(v.window_point(-1.0, 50.0), None);
    }

    #[test]
    fn small_window_is_not_magnified() {
        let v = Viewport::fit(rect(0.0, 0.0, 400.0, 300.0), 1.0, 1280);
        assert_eq!(v.scale, 1.0);
        assert_eq!(v.image_size(), (400, 300));
    }

    #[test]
    fn zoomed_region_is_magnified_and_offset() {
        let v = Viewport::fit(rect(100.0, 50.0, 200.0, 40.0), 2.0, 1280);
        assert_eq!(v.scale, 2.0);
        assert_eq!(v.image_size(), (400, 80));
        assert_eq!(v.to_image(rect(110.0, 60.0, 10.0, 10.0)), rect(20.0, 20.0, 20.0, 20.0));
    }

    #[test]
    fn thumbnail_is_a_window_listed_much_smaller_than_it_is() {
        let real = rect(461.0, 76.0, 673.0, 439.0);
        assert!(is_thumbnail(rect(-240.0, 549.0, 121.0, 102.0), real));
        assert!(!is_thumbnail(real, real));
        assert!(!is_thumbnail(rect(461.0, 76.0, 672.0, 438.0), real), "rounding is not shrinking");
    }

    #[test]
    fn clamp_cuts_overhang_and_rejects_outside() {
        assert_eq!(
            clamp_to_window(rect(-10.0, 280.0, 50.0, 50.0), 400.0, 300.0),
            Some(rect(0.0, 280.0, 40.0, 20.0))
        );
        assert_eq!(clamp_to_window(rect(500.0, 0.0, 10.0, 10.0), 400.0, 300.0), None);
    }
}
