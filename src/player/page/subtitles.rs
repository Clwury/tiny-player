use super::*;
use crate::media::{SUBTITLE_VERTICAL_ADJUST_STEP_FRACTION, SubtitleAppearance, SubtitleStyle};
use gpui::{AnyElement, App, AvailableSpace, Element, GlobalElementId, InspectorElementId};
use std::{cell::Cell, rc::Rc};

pub(super) const SUBTITLE_SCALE_STEP: f32 = 0.1;

impl PlaybackPage {
    pub(super) fn toggle_subtitle_visibility(&mut self, cx: &mut Context<Self>) {
        self.presentation.subtitle.hidden = !self.presentation.subtitle.hidden;
        self.presentation.subtitle.visible_offset_limit.set(None);
        cx.notify();
    }

    pub(super) fn adjust_subtitle_vertical_offset_fraction(
        &mut self,
        delta: f32,
        cx: &mut Context<Self>,
    ) {
        let current_offset_fraction = self
            .presentation
            .subtitle
            .vertical_offset_fraction
            .unwrap_or(0.0);
        let limit = self.presentation.subtitle.visible_offset_limit.get();
        let next_offset_fraction = adjusted_subtitle_offset(current_offset_fraction, delta, limit);
        if self.presentation.subtitle.vertical_offset_fraction == next_offset_fraction {
            return;
        }
        self.presentation.subtitle.vertical_offset_fraction = next_offset_fraction;
        cx.emit(PlaybackEvent::SubtitleVerticalPositionChanged {
            fraction: next_offset_fraction,
        });
        cx.notify();
    }

    pub(super) fn adjust_subtitle_scale(&mut self, delta: f32, cx: &mut Context<Self>) {
        let current = normalized_subtitle_scale(self.subtitle_scale);
        self.subtitle_scale = normalized_subtitle_scale(current + delta);
        self.presentation.subtitle.visible_offset_limit.set(None);
        let label = crate::i18n::tr(format!("字幕大小：{:.2}×", self.subtitle_scale), cx);
        self.show_playback_feedback(PlaybackFeedback::SubtitleScale(label), cx);
    }

    pub(super) fn render_subtitle_overlay(&self, cx: &gpui::App) -> impl IntoElement {
        if self.presentation.subtitle.hidden {
            return div()
                .id("playback-subtitle-overlay-empty")
                .into_any_element();
        }
        let Some(cue) = self.presentation.subtitle.active.as_ref() else {
            return div()
                .id("playback-subtitle-overlay-empty")
                .into_any_element();
        };
        let Some(observed_video_bounds) = self.presentation.frame.viewport_bounds else {
            return div()
                .id("playback-subtitle-overlay-empty")
                .into_any_element();
        };
        // Canvas observations are window-relative; subtitle children are laid
        // out inside the same adaptive video viewport as the image.
        let video_bounds = local_video_viewport_bounds(observed_video_bounds);
        let Some(source_size) = self.presentation.frame.source_size else {
            return div()
                .id("playback-subtitle-overlay-empty")
                .into_any_element();
        };
        let Some(video_fitted_bounds) = aspect_fit_bounds(video_bounds, source_size) else {
            return div()
                .id("playback-subtitle-overlay-empty")
                .into_any_element();
        };

        if !cue.has_content() {
            return div()
                .id("playback-subtitle-overlay-empty")
                .into_any_element();
        }

        let subtitle_scale = normalized_subtitle_scale(self.subtitle_scale);
        let appearance = SubtitleStyle::get(cx).appearance;
        let bitmap_canvas_size = subtitle_bitmap_canvas_size(cue).unwrap_or(source_size);
        let bitmap_bounds =
            aspect_fit_bounds(video_bounds, bitmap_canvas_size).unwrap_or(video_fitted_bounds);
        let bitmap_bounds = scaled_subtitle_bounds(bitmap_bounds, subtitle_scale);
        let scale_x = bitmap_bounds.size.width / px(bitmap_canvas_size.width as f32);
        let scale_y = bitmap_bounds.size.height / px(bitmap_canvas_size.height as f32);
        let subtitle_bottom = subtitle_video_bottom(video_fitted_bounds);
        let subtitle_render_bottom = subtitle_render_bottom(
            video_fitted_bounds,
            subtitle_bottom,
            self.presentation.subtitle.vertical_offset_fraction,
        );
        let manual_position = self
            .presentation
            .subtitle
            .vertical_offset_fraction
            .is_some();
        let bitmap_top = if manual_position {
            subtitle_bitmap_overlay_top_for_bottom(
                cue,
                bitmap_bounds,
                scale_y,
                subtitle_render_bottom,
                video_bounds.size.height,
            )
        } else {
            let bitmap_bottom_offset =
                subtitle_bitmap_bottom_offset(cue, bitmap_bounds, scale_y, subtitle_bottom);
            subtitle_bitmap_overlay_top(bitmap_bounds, bitmap_bottom_offset)
        };
        let bitmap_overlay = cue.bitmaps.iter().fold(
            div()
                .id("playback-subtitle-bitmap-overlay")
                .debug_selector(|| "playback-subtitle-bitmap-overlay".to_string())
                .absolute()
                .left(bitmap_bounds.origin.x)
                .top(bitmap_top)
                .w(bitmap_bounds.size.width)
                .h(bitmap_bounds.size.height),
            |this, bitmap| {
                let image = self
                    .presentation
                    .subtitle
                    .images
                    .get(&bitmap.image)
                    .expect("active subtitle images are prepared when the cue changes");
                this.child(render_subtitle_bitmap(
                    bitmap,
                    image.clone(),
                    scale_x,
                    scale_y,
                ))
            },
        );
        let overlay = div()
            .id("playback-subtitle-overlay")
            .absolute()
            .left_0()
            .top_0()
            .w_full()
            .h_full()
            .child(bitmap_overlay);

        let limit = &self.presentation.subtitle.visible_offset_limit;
        if cue.text.trim().is_empty() {
            limit.set(
                subtitle_bitmap_content_span(cue, scale_y).map(|(top, bottom)| {
                    visible_subtitle_offset_limit(
                        subtitle_bottom,
                        video_fitted_bounds.size.height,
                        bottom - top,
                        px(0.0),
                    )
                }),
            );
            return overlay.into_any_element();
        }

        let text_overlay_bounds = subtitle_text_overlay_bounds(
            video_fitted_bounds,
            subtitle_render_bottom,
            self.presentation.subtitle.vertical_offset_fraction,
        );
        let text = div()
            .id("playback-subtitle-text-overlay")
            .debug_selector(|| "playback-subtitle-text-overlay".to_string())
            .w(text_overlay_bounds.size.width)
            .flex()
            .justify_center()
            .items_end()
            .px_6()
            .child(
                div()
                    .debug_selector(|| "playback-subtitle-text".to_string())
                    .max_w(relative(0.86))
                    .px_3()
                    .when_some(appearance.background.rgba_hex(), |this, color| {
                        this.rounded(px(2.0)).bg(rgba(color))
                    })
                    .child(render_subtitle_text(
                        cue.text.clone(),
                        appearance,
                        subtitle_scale,
                    )),
            );
        if manual_position {
            overlay.child(ClampedSubtitleText {
                content: text.into_any_element(),
                left: text_overlay_bounds.origin.x,
                width: text_overlay_bounds.size.width,
                bottom: subtitle_render_bottom,
                outline: px(appearance.outline.width_px(subtitle_scale)),
                offset_limit: Some(SubtitleOffsetMeasurement {
                    limit: limit.clone(),
                    video_bottom: subtitle_bottom,
                    video_height: video_fitted_bounds.size.height,
                }),
            })
        } else {
            limit.set(None);
            overlay.child(
                text.absolute()
                    .left(text_overlay_bounds.origin.x)
                    .top(text_overlay_bounds.origin.y)
                    .h(text_overlay_bounds.size.height),
            )
        }
        .into_any_element()
    }
}

/// Measure wrapped text in the same frame in which we constrain its position.
struct ClampedSubtitleText {
    content: AnyElement,
    left: Pixels,
    width: Pixels,
    bottom: Pixels,
    outline: Pixels,
    offset_limit: Option<SubtitleOffsetMeasurement>,
}

struct SubtitleOffsetMeasurement {
    limit: Rc<Cell<Option<f32>>>,
    video_bottom: Pixels,
    video_height: Pixels,
}

impl IntoElement for ClampedSubtitleText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ClampedSubtitleText {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, ()) {
        let style = gpui::Style {
            size: gpui::size(gpui::relative(1.0).into(), gpui::relative(1.0).into()),
            position: gpui::Position::Absolute,
            inset: gpui::Edges::all(px(0.0).into()),
            ..Default::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let space = gpui::size(self.width.into(), AvailableSpace::MaxContent);
        let height = self.content.layout_as_root(space, window, cx).height;
        if let Some(measurement) = &self.offset_limit {
            measurement.limit.set(Some(visible_subtitle_offset_limit(
                measurement.video_bottom,
                measurement.video_height,
                height,
                self.outline,
            )));
        }
        let top = clamped_subtitle_top(self.bottom, height, bounds.size.height, self.outline);
        self.content.prepaint_at(
            gpui::point(bounds.origin.x + self.left, bounds.origin.y + top),
            window,
            cx,
        );
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.content.paint(window, cx);
    }
}

fn clamped_subtitle_top(
    bottom: Pixels,
    height: Pixels,
    viewport_height: Pixels,
    inset: Pixels,
) -> Pixels {
    let last_top = viewport_height - inset - height;
    // An oversized cue cannot fit; show its beginning rather than hiding it.
    (bottom - height).max(inset).min(last_top.max(inset))
}

const SUBTITLE_SCALE_MIN: f32 = 0.5;
const SUBTITLE_SCALE_MAX: f32 = 3.0;

pub(super) fn normalized_subtitle_scale(scale: f32) -> f32 {
    if scale.is_finite() {
        scale.clamp(SUBTITLE_SCALE_MIN, SUBTITLE_SCALE_MAX)
    } else {
        1.0
    }
}

pub(super) fn scaled_subtitle_bounds(bounds: Bounds<Pixels>, scale: f32) -> Bounds<Pixels> {
    if (scale - 1.0).abs() < f32::EPSILON {
        return bounds;
    }
    let width = bounds.size.width * scale;
    let height = bounds.size.height * scale;
    Bounds::new(
        gpui::point(
            bounds.origin.x + (bounds.size.width - width) / 2.0,
            bounds.origin.y + bounds.size.height - height,
        ),
        gpui::size(width, height),
    )
}

pub(super) fn render_subtitle_bitmap(
    bitmap: &BackendSubtitleBitmap,
    image: Arc<RenderImage>,
    scale_x: f32,
    scale_y: f32,
) -> impl IntoElement {
    div()
        .absolute()
        .left(px(bitmap.x as f32) * scale_x)
        .top(px(bitmap.y as f32) * scale_y)
        .w(px(bitmap.width as f32) * scale_x)
        .h(px(bitmap.height as f32) * scale_y)
        .child(SubtitleBitmapElement { image })
}

pub(super) fn local_video_viewport_bounds(bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds::new(gpui::point(px(0.0), px(0.0)), bounds.size)
}

pub(super) fn subtitle_video_bottom(video_fitted_bounds: Bounds<Pixels>) -> Pixels {
    video_fitted_bounds.origin.y + video_fitted_bounds.size.height
}

pub(super) fn subtitle_render_bottom(
    video_fitted_bounds: Bounds<Pixels>,
    default_subtitle_bottom: Pixels,
    subtitle_vertical_offset_fraction: Option<f32>,
) -> Pixels {
    subtitle_vertical_offset_fraction.map_or(default_subtitle_bottom, |offset_fraction| {
        subtitle_video_bottom(video_fitted_bounds)
            - subtitle_vertical_offset_pixels(video_fitted_bounds, offset_fraction)
    })
}

pub(super) fn subtitle_text_overlay_bounds(
    video_fitted_bounds: Bounds<Pixels>,
    subtitle_bottom: Pixels,
    subtitle_vertical_offset_fraction: Option<f32>,
) -> Bounds<Pixels> {
    let height = if subtitle_vertical_offset_fraction.is_some() {
        video_fitted_bounds.size.height
    } else {
        subtitle_text_overlay_height_for_bottom(video_fitted_bounds, subtitle_bottom)
    };

    Bounds::new(
        gpui::point(video_fitted_bounds.origin.x, subtitle_bottom - height),
        gpui::size(video_fitted_bounds.size.width, height),
    )
}

pub(super) fn subtitle_bitmap_overlay_top(
    bitmap_bounds: Bounds<Pixels>,
    bitmap_bottom_offset: Pixels,
) -> Pixels {
    bitmap_bounds.origin.y - bitmap_bottom_offset
}

fn subtitle_bitmap_overlay_top_for_bottom(
    cue: &BackendSubtitleCue,
    bitmap_bounds: Bounds<Pixels>,
    scale_y: f32,
    subtitle_bottom: Pixels,
    viewport_height: Pixels,
) -> Pixels {
    let Some((top, bottom)) = subtitle_bitmap_content_span(cue, scale_y) else {
        return bitmap_bounds.origin.y;
    };
    clamped_subtitle_top(subtitle_bottom, bottom - top, viewport_height, px(0.0)) - top
}

pub(super) fn subtitle_vertical_offset_after_adjustment(current_offset: f32, delta: f32) -> f32 {
    current_offset + delta
}

fn visible_subtitle_offset_limit(
    video_bottom: Pixels,
    video_height: Pixels,
    cue_height: Pixels,
    outline: Pixels,
) -> f32 {
    ((video_bottom - cue_height - outline) / video_height).clamp(0.0, 1.0)
}

fn adjusted_subtitle_offset(current: f32, delta: f32, limit: Option<f32>) -> Option<f32> {
    let Some(limit) = limit else {
        return stepped_subtitle_vertical_offset(current, delta);
    };
    if delta > 0.0 && current >= limit {
        // Keep the preference when the cue is already at its visible ceiling.
        return SubtitleStyle::normalize_vertical_offset(Some(current));
    }
    stepped_subtitle_vertical_offset(current.min(limit), delta)
        .and_then(|next| SubtitleStyle::normalize_vertical_offset(Some(next.min(limit))))
}

/// Snap adjustments to the 1% grid so non-grid values loaded from disk can
/// always reach the default position instead of oscillating around it.
fn stepped_subtitle_vertical_offset(current_offset: f32, delta: f32) -> Option<f32> {
    let raw = subtitle_vertical_offset_after_adjustment(current_offset, delta);
    let stepped = (raw / SUBTITLE_VERTICAL_ADJUST_STEP_FRACTION).round()
        * SUBTITLE_VERTICAL_ADJUST_STEP_FRACTION;
    SubtitleStyle::normalize_vertical_offset(Some(stepped))
}

/// Maximum spacing between neighbouring samples on one outline ring.
/// Shorter chords than this keep thick outlines continuous around corners and
/// thin stems instead of leaving scalloped edges.
const SUBTITLE_OUTLINE_SAMPLE_CHORD_PX: f32 = 1.0;
/// Outlines thicker than this get extra rings so the band is filled evenly.
const SUBTITLE_OUTLINE_RING_SPACING_PX: f32 = 1.5;

/// Samples one ring at `radius`, with `staggered` shifting the angles by half
/// a step so neighbouring rings do not line up their gaps.
fn outline_ring_samples(radius: f32, staggered: bool) -> impl Iterator<Item = (f32, f32)> {
    let circumference = std::f32::consts::TAU * radius;
    let count = (circumference / SUBTITLE_OUTLINE_SAMPLE_CHORD_PX).ceil() as usize;
    let count = count.clamp(8, 128);
    let step = std::f32::consts::TAU / count as f32;
    let phase = if staggered { step / 2.0 } else { 0.0 };
    (0..count).map(move |index| {
        let angle = phase + index as f32 * step;
        (radius * angle.cos(), radius * angle.sin())
    })
}

/// Sample circular rings so an outline fills the band between the glyph and
/// its outer edge instead of drawing a detached duplicate.
pub(super) fn subtitle_outline_offsets(width: f32) -> impl Iterator<Item = (f32, f32)> {
    let width = if width.is_finite() {
        width.max(0.0)
    } else {
        0.0
    };
    let rings = (width / SUBTITLE_OUTLINE_RING_SPACING_PX).ceil() as usize;
    (0..rings).flat_map(move |index| {
        let radius = width * (index + 1) as f32 / rings as f32;
        outline_ring_samples(radius, index % 2 == 1)
    })
}

fn cached_subtitle_outline_offsets(width: f32) -> &'static [(f32, f32)] {
    use std::sync::LazyLock;

    static OFFSETS: LazyLock<[Vec<(f32, f32)>; 4]> = LazyLock::new(|| {
        [1.5, 2.0, 2.5, 3.0].map(|width| subtitle_outline_offsets(width).collect())
    });
    match width {
        1.5 => &OFFSETS[0],
        2.0 => &OFFSETS[1],
        2.5 => &OFFSETS[2],
        3.0 => &OFFSETS[3],
        _ => &[],
    }
}

fn render_subtitle_text(
    text: String,
    appearance: SubtitleAppearance,
    scale: f32,
) -> impl IntoElement {
    let text: SharedString = text.into();
    let text_size = px(30.0 * scale);
    let line_height = px(36.0 * scale);
    let text_layer = || {
        div()
            .text_size(text_size)
            .line_height(line_height)
            .text_center()
    };
    let mut stack = div().relative();
    let outline_width = appearance.outline.width_px(scale);
    if outline_width > 0.0 {
        for &(x, y) in cached_subtitle_outline_offsets(outline_width) {
            stack = stack.child(
                text_layer()
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w_full()
                    .text_color(rgb(0x000000))
                    .child(text.clone()),
            );
        }
    }
    stack.child(
        text_layer()
            .text_color(rgb(appearance.text_color.rgb_hex()))
            .child(text),
    )
}

pub(super) fn subtitle_vertical_adjust_step() -> f32 {
    SUBTITLE_VERTICAL_ADJUST_STEP_FRACTION
}

pub(super) fn subtitle_vertical_offset_pixels(
    video_fitted_bounds: Bounds<Pixels>,
    offset_fraction: f32,
) -> Pixels {
    video_fitted_bounds.size.height * offset_fraction
}

pub(super) fn subtitle_text_overlay_height_for_bottom(
    video_fitted_bounds: Bounds<Pixels>,
    bottom: Pixels,
) -> Pixels {
    let top = video_fitted_bounds.origin.y;

    if bottom > top { bottom - top } else { px(0.0) }
}

pub(super) fn subtitle_bitmap_bottom_offset(
    cue: &BackendSubtitleCue,
    bitmap_bounds: Bounds<Pixels>,
    scale_y: f32,
    subtitle_bottom: Pixels,
) -> Pixels {
    let content_bottom = subtitle_bitmap_content_bottom(cue, bitmap_bounds, scale_y);

    content_bottom
        .filter(|content_bottom| *content_bottom > subtitle_bottom)
        .map(|content_bottom| content_bottom - subtitle_bottom)
        .unwrap_or(px(0.0))
}

pub(super) fn subtitle_bitmap_content_bottom(
    cue: &BackendSubtitleCue,
    bitmap_bounds: Bounds<Pixels>,
    scale_y: f32,
) -> Option<Pixels> {
    subtitle_bitmap_content_span(cue, scale_y).map(|(_, bottom)| bitmap_bounds.origin.y + bottom)
}

fn subtitle_bitmap_content_span(
    cue: &BackendSubtitleCue,
    scale_y: f32,
) -> Option<(Pixels, Pixels)> {
    cue.bitmaps.iter().fold(None, |span, bitmap| {
        let top = px(bitmap.y as f32) * scale_y;
        let bottom = px(bitmap.y.saturating_add(bitmap.height) as f32) * scale_y;
        Some(
            span.map_or((top, bottom), |(first, last): (Pixels, Pixels)| {
                (first.min(top), last.max(bottom))
            }),
        )
    })
}

pub(super) fn subtitle_bitmap_canvas_size(cue: &BackendSubtitleCue) -> Option<RenderSize> {
    cue.bitmaps
        .iter()
        .filter(|bitmap| bitmap.canvas_width > 0 && bitmap.canvas_height > 0)
        .fold(None, |size, bitmap| {
            Some(match size {
                Some(size) => RenderSize {
                    width: size.width.max(bitmap.canvas_width),
                    height: size.height.max(bitmap.canvas_height),
                },
                None => RenderSize {
                    width: bitmap.canvas_width,
                    height: bitmap.canvas_height,
                },
            })
        })
}

pub(super) fn defer_drop_subtitle(subtitle: &mut SubtitleOverlayState, window: &mut Window) {
    subtitle.active = None;
    subtitle.visible_offset_limit.set(None);
    for image in subtitle.images.update(None) {
        defer_drop_frame(image, window);
    }
}

struct SubtitleBitmapElement {
    image: Arc<RenderImage>,
}

impl gpui::Element for SubtitleBitmapElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let style = gpui::Style {
            size: gpui::Size {
                width: gpui::Length::Definite(gpui::DefiniteLength::Fraction(1.0)),
                height: gpui::Length::Definite(gpui::DefiniteLength::Fraction(1.0)),
            },
            ..Default::default()
        };

        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut gpui::App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut gpui::App,
    ) {
        _ = window.paint_image(
            bounds,
            bounds,
            gpui::Corners::default(),
            self.image.clone(),
            0,
            false,
        );
    }
}

impl IntoElement for SubtitleBitmapElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, point, px, size};

    use tiny_playback::{
        BackendSubtitleBitmap, BackendSubtitleCue, BgraImage, RenderSize, SharedBgraImage,
    };

    use super::*;

    #[gpui::test]
    fn manual_wrapped_text_stays_visible_after_resize(cx: &mut gpui::TestAppContext) {
        struct WrappedSubtitle;

        impl Render for WrappedSubtitle {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .relative()
                    .size_full()
                    .debug_selector(|| "subtitle-test-viewport".to_string())
                    .child(ClampedSubtitleText {
                        content: div()
                            .w(px(240.0))
                            .debug_selector(|| "subtitle-test-cue".to_string())
                            .child(render_subtitle_text(
                                "A wrapped subtitle with enough words to take several lines."
                                    .into(),
                                SubtitleAppearance::default(),
                                1.0,
                            ))
                            .into_any_element(),
                        left: px(0.0),
                        width: px(240.0),
                        bottom: px(10.0),
                        outline: px(2.0),
                        offset_limit: None,
                    })
            }
        }

        let (_, cx) = cx.add_window_view(|_, _| WrappedSubtitle);
        for height in [480.0, 280.0] {
            cx.simulate_resize(size(px(640.0), px(height)));
            cx.run_until_parked();
            let viewport = cx.debug_bounds("subtitle-test-viewport").unwrap();
            let cue = cx.debug_bounds("subtitle-test-cue").unwrap();
            assert!(cue.size.height > px(36.0));
            assert_eq!(cue.top(), viewport.top() + px(2.0));
            assert!(cue.bottom() <= viewport.bottom() - px(2.0));
        }
    }

    #[test]
    fn manual_subtitle_position_restores_when_letterbox_space_returns() {
        let letterboxed = Bounds::new(point(px(0.0), px(75.0)), size(px(800.0), px(450.0)));
        let full = Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(600.0)));
        let saved_offset = Some(1.0);
        let top = |video| {
            let bottom = subtitle_render_bottom(video, subtitle_video_bottom(video), saved_offset);
            clamped_subtitle_top(bottom, px(36.0), px(600.0), px(2.0))
        };
        assert_eq!(top(letterboxed), px(39.0));
        assert_eq!(top(full), px(2.0));
        assert_eq!(top(letterboxed), px(39.0));
        assert_eq!(
            clamped_subtitle_top(px(20.0), px(700.0), px(600.0), px(2.0)),
            px(2.0)
        );
    }

    #[test]
    fn local_video_viewport_bounds_strips_window_origin_for_overlay_layout() {
        let observed = Bounds::new(
            point(px(0.0), px(1043.3334)),
            size(px(1485.3334), px(1008.0)),
        );

        assert_eq!(
            local_video_viewport_bounds(observed),
            Bounds::new(point(px(0.0), px(0.0)), observed.size)
        );
    }

    #[test]
    fn subtitle_text_overlay_uses_full_video_bounds() {
        for video_fitted_bounds in [
            Bounds::new(point(px(0.0), px(75.0)), size(px(800.0), px(450.0))),
            Bounds::new(point(px(10.0), px(95.0)), size(px(800.0), px(450.0))),
            Bounds::new(point(px(0.0), px(0.0)), size(px(1600.0), px(900.0))),
        ] {
            assert_eq!(
                subtitle_text_overlay_bounds(
                    video_fitted_bounds,
                    subtitle_video_bottom(video_fitted_bounds),
                    None,
                ),
                video_fitted_bounds,
            );
        }
    }

    #[test]
    fn subtitle_text_overlay_offset_shifts_position_without_changing_default_height() {
        let video_fitted_bounds = Bounds::new(point(px(0.0), px(75.0)), size(px(800.0), px(450.0)));

        let raised = subtitle_text_overlay_bounds(video_fitted_bounds, px(25.0), Some(1.0));
        assert_eq!(raised.origin.y, px(-425.0));
        assert_eq!(raised.size.height, px(450.0));
    }

    #[test]
    fn subtitle_render_bottom_adjusts_from_video_bottom() {
        let video_fitted_bounds = Bounds::new(point(px(0.0), px(75.0)), size(px(800.0), px(450.0)));
        let default_bottom = subtitle_video_bottom(video_fitted_bounds);
        let manual_offset =
            subtitle_vertical_offset_after_adjustment(0.0, subtitle_vertical_adjust_step());

        assert_eq!(
            subtitle_render_bottom(video_fitted_bounds, default_bottom, None),
            px(525.0)
        );
        assert_eq!(
            subtitle_render_bottom(video_fitted_bounds, default_bottom, Some(manual_offset)),
            px(520.5)
        );
    }

    #[test]
    fn cached_outline_offsets_match_the_width_presets() {
        for scale in [0.5, 1.0, 1.5, 2.0] {
            let width = crate::media::SubtitleOutline::On.width_px(scale);
            let expected: Vec<_> = subtitle_outline_offsets(width).collect();
            assert!(!expected.is_empty());
            assert_eq!(cached_subtitle_outline_offsets(width), expected.as_slice());
        }
    }

    #[test]
    fn subtitle_outline_offsets_fill_each_ring_without_gaps() {
        assert_eq!(subtitle_outline_offsets(0.0).count(), 0);
        assert_eq!(subtitle_outline_offsets(1.0).count(), 8);
        assert_eq!(subtitle_outline_offsets(1.5).count(), 10);
        assert_eq!(subtitle_outline_offsets(2.0).count(), 21);
        assert_eq!(subtitle_outline_offsets(2.5).count(), 24);
        assert_eq!(subtitle_outline_offsets(3.0).count(), 29);

        let offsets: Vec<_> = subtitle_outline_offsets(3.0).collect();
        let radius = |(x, y): &(f32, f32)| (x * x + y * y).sqrt();
        let inner = offsets
            .iter()
            .filter(|o| (radius(o) - 1.5).abs() < 1e-4)
            .count();
        let outer = offsets
            .iter()
            .filter(|o| (radius(o) - 3.0).abs() < 1e-4)
            .count();
        assert_eq!((inner, outer), (10, 19));
        // Circular rings keep the stroke round: no 8-direction diagonal reach
        // beyond the requested width, unlike square grids.
        assert!(offsets.iter().all(|o| radius(o) <= 3.0 + 1e-4));
    }

    #[test]
    fn subtitle_vertical_steps_snap_to_grid_and_reach_default() {
        assert_eq!(stepped_subtitle_vertical_offset(0.0, 0.01), Some(0.01));
        assert_eq!(stepped_subtitle_vertical_offset(0.01, -0.01), None);
        assert_eq!(stepped_subtitle_vertical_offset(0.0051, -0.01), None);
        assert_eq!(stepped_subtitle_vertical_offset(0.0051, 0.01), Some(0.02));
        assert_eq!(stepped_subtitle_vertical_offset(0.0149, 0.01), Some(0.02));
        assert_eq!(stepped_subtitle_vertical_offset(0.02, -0.01), Some(0.01));
    }

    #[test]
    fn subtitle_vertical_adjust_step_uses_one_percent_of_video_height() {
        let video_fitted_bounds = Bounds::new(point(px(0.0), px(75.0)), size(px(800.0), px(450.0)));

        assert_eq!(subtitle_vertical_adjust_step(), 0.01);
        assert_eq!(
            subtitle_vertical_offset_pixels(video_fitted_bounds, subtitle_vertical_adjust_step()),
            px(4.5)
        );
    }

    #[test]
    fn subtitle_vertical_offset_fraction_keeps_relative_position_after_resize() {
        let compact_video = Bounds::new(point(px(0.0), px(75.0)), size(px(800.0), px(450.0)));
        let fullscreen_video = Bounds::new(point(px(0.0), px(0.0)), size(px(1600.0), px(900.0)));
        let offset_fraction = subtitle_vertical_offset_after_adjustment(43.0 / 450.0, 0.01);

        assert_eq!(
            subtitle_render_bottom(compact_video, px(0.0), Some(offset_fraction)),
            px(477.5)
        );
        assert_eq!(
            subtitle_render_bottom(fullscreen_video, px(0.0), Some(offset_fraction)),
            px(805.0)
        );
    }

    #[test]
    fn manual_bitmap_position_moves_all_regions_together() {
        let bitmap_bounds = Bounds::new(point(px(0.0), px(75.0)), size(px(800.0), px(450.0)));
        let image = SharedBgraImage::new(BgraImage::new(vec![0, 0, 0, 0], 1, 1).unwrap());
        let mut cue = BackendSubtitleCue {
            text: String::new(),
            bitmaps: vec![BackendSubtitleBitmap {
                image,
                x: 0,
                y: 900,
                width: 100,
                height: 100,
                canvas_width: 1920,
                canvas_height: 1080,
            }],
            start_nsecs: 0,
            end_nsecs: 1_000_000_000,
        };

        let mut upper_region = cue.bitmaps[0].clone();
        upper_region.y = 800;
        cue.bitmaps.push(upper_region);

        assert_eq!(
            subtitle_bitmap_overlay_top_for_bottom(&cue, bitmap_bounds, 1.0, px(575.0), px(600.0)),
            px(-425.0)
        );
        assert_eq!(
            subtitle_bitmap_overlay_top_for_bottom(&cue, bitmap_bounds, 1.0, px(1035.0), px(600.0)),
            px(-400.0)
        );
        assert_eq!(
            subtitle_bitmap_overlay_top_for_bottom(&cue, bitmap_bounds, 1.0, px(50.0), px(600.0)),
            px(-800.0)
        );
    }

    #[test]
    fn reversing_at_the_visible_ceiling_moves_on_the_first_press() {
        let limit = Some(0.253);
        let raised = adjusted_subtitle_offset(0.25, 0.01, limit);
        assert_eq!(raised, limit);
        assert_eq!(adjusted_subtitle_offset(0.253, 0.01, limit), limit);
        assert_eq!(adjusted_subtitle_offset(0.95, 0.01, limit), Some(0.95));
        for offset in [0.253, 0.95] {
            let lowered = adjusted_subtitle_offset(offset, -0.01, limit).unwrap();
            assert!((lowered - 0.24).abs() < 0.0001);
        }
        assert_eq!(adjusted_subtitle_offset(0.01, -0.01, limit), None);
    }

    #[test]
    fn lowering_subtitles_stops_at_default_position() {
        assert_eq!(stepped_subtitle_vertical_offset(0.0, -0.01), None);
        assert_eq!(stepped_subtitle_vertical_offset(0.02, -0.03), None);
    }

    #[test]
    fn subtitle_bitmap_bottom_offset_lifts_only_overlapping_bitmap_content() {
        let image = SharedBgraImage::new(BgraImage::new(vec![0, 0, 0, 0], 1, 1).unwrap());
        let cue = BackendSubtitleCue {
            text: String::new(),
            bitmaps: vec![BackendSubtitleBitmap {
                image,
                x: 0,
                y: 900,
                width: 100,
                height: 100,
                canvas_width: 1920,
                canvas_height: 1080,
            }],
            start_nsecs: 0,
            end_nsecs: 1_000_000_000,
        };
        let bitmap_bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(1920.0), px(1080.0)));

        assert_eq!(
            subtitle_bitmap_bottom_offset(&cue, bitmap_bounds, 1.0, px(960.0)),
            px(40.0)
        );
        assert_eq!(
            subtitle_bitmap_bottom_offset(&cue, bitmap_bounds, 1.0, px(1000.0)),
            px(0.0)
        );
    }

    #[test]
    fn subtitle_bitmap_canvas_size_uses_largest_bitmap_canvas() {
        let image = SharedBgraImage::new(BgraImage::new(vec![0, 0, 0, 0], 1, 1).unwrap());
        let cue = BackendSubtitleCue {
            text: String::new(),
            bitmaps: vec![
                BackendSubtitleBitmap {
                    image: image.clone(),
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                    canvas_width: 1920,
                    canvas_height: 800,
                },
                BackendSubtitleBitmap {
                    image,
                    x: 0,
                    y: 900,
                    width: 1,
                    height: 1,
                    canvas_width: 1920,
                    canvas_height: 1080,
                },
            ],
            start_nsecs: 0,
            end_nsecs: 1_000_000_000,
        };

        assert_eq!(
            subtitle_bitmap_canvas_size(&cue),
            Some(RenderSize {
                width: 1920,
                height: 1080,
            })
        );
    }

    #[test]
    fn subtitle_scale_is_clamped_to_supported_range() {
        assert_eq!(normalized_subtitle_scale(f32::NAN), 1.0);
        assert_eq!(normalized_subtitle_scale(f32::INFINITY), 1.0);
        assert_eq!(normalized_subtitle_scale(0.1), SUBTITLE_SCALE_MIN);
        assert_eq!(normalized_subtitle_scale(9.0), SUBTITLE_SCALE_MAX);
        assert_eq!(normalized_subtitle_scale(1.25), 1.25);
    }

    #[test]
    fn scaled_subtitle_bounds_grows_from_bottom_centre() {
        let bounds = Bounds::new(point(px(0.0), px(100.0)), size(px(200.0), px(50.0)));

        assert_eq!(scaled_subtitle_bounds(bounds, 1.0), bounds);

        let scaled = scaled_subtitle_bounds(bounds, 1.5);
        assert_eq!(scaled.size, size(px(300.0), px(75.0)));
        assert_eq!(scaled.origin, point(px(-50.0), px(75.0)));
        assert_eq!(
            scaled.origin.y + scaled.size.height,
            bounds.origin.y + bounds.size.height
        );
        assert_eq!(
            scaled.origin.x + scaled.size.width / 2.0,
            bounds.origin.x + bounds.size.width / 2.0
        );
    }
}
