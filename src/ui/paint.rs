use gpui::{Bounds, ContentMask, Pixels, Window, point, px};

/// Use the same edge snapping as GPUI's image and quad primitives.
pub(crate) fn device_bounds(window: &Window, bounds: Bounds<Pixels>) -> Bounds<i32> {
    let snap = |value| (f32::from(window.pixel_snap(value)) * window.scale_factor()).round() as i32;
    Bounds::from_corners(
        point(snap(bounds.left()), snap(bounds.top())),
        point(snap(bounds.right()), snap(bounds.bottom())),
    )
}

pub(crate) fn device_mask(bounds: Bounds<i32>, scale: f32) -> ContentMask<Pixels> {
    // GPUI floors/ceils mask edges. Move them inside their boundary pixels so
    // division by a fractional scale cannot expand a mask into its neighbor.
    // Rasterized coverage is unchanged: all pixel centers remain inside.
    ContentMask {
        bounds: Bounds::from_corners(
            point(
                px((bounds.left() as f32 + 0.25) / scale),
                px((bounds.top() as f32 + 0.25) / scale),
            ),
            point(
                px((bounds.right() as f32 - 0.25) / scale),
                px((bounds.bottom() as f32 - 0.25) / scale),
            ),
        ),
    }
}
