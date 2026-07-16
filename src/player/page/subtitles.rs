use super::fullscreen::playback_progress_bar_bounds;
use super::*;

const SUBTITLE_VERTICAL_ADJUST_STEP_FRACTION: f32 = 0.01;

impl PlaybackPage {
    pub(super) fn adjust_subtitle_vertical_offset_fraction(
        &mut self,
        delta: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let current_offset_fraction = self.current_subtitle_vertical_offset_fraction(window);
        self.subtitle.vertical_offset_fraction = Some(subtitle_vertical_offset_after_adjustment(
            current_offset_fraction,
            delta,
        ));
        cx.notify();
    }

    pub(super) fn current_subtitle_vertical_offset_fraction(&self, window: &Window) -> f32 {
        self.subtitle
            .vertical_offset_fraction
            .or_else(|| self.default_subtitle_vertical_offset_fraction(window))
            .unwrap_or(0.0)
    }

    pub(super) fn default_subtitle_vertical_offset_fraction(
        &self,
        _window: &Window,
    ) -> Option<f32> {
        let (video_bounds, video_fitted_bounds) = self.current_video_layout_bounds()?;
        let default_bottom = subtitle_overlay_bottom(
            video_fitted_bounds,
            video_bounds,
            self.progress_bar_visible(),
        );

        Some(subtitle_vertical_offset_fraction(
            video_fitted_bounds,
            subtitle_video_bottom(video_fitted_bounds) - default_bottom,
        ))
    }

    pub(super) fn current_video_layout_bounds(&self) -> Option<(Bounds<Pixels>, Bounds<Pixels>)> {
        let video_bounds = local_video_viewport_bounds(self.frame.viewport_bounds?);
        let video_fitted_bounds = aspect_fit_bounds(video_bounds, self.frame.source_size?)?;
        Some((video_bounds, video_fitted_bounds))
    }

    pub(super) fn render_subtitle_overlay(&self, progress_bar_visible: bool) -> impl IntoElement {
        let Some(cue) = self.subtitle.active.as_ref() else {
            return div()
                .id("playback-subtitle-overlay-empty")
                .into_any_element();
        };
        let Some(observed_video_bounds) = self.frame.viewport_bounds else {
            return div()
                .id("playback-subtitle-overlay-empty")
                .into_any_element();
        };
        // Canvas observations are window-relative, while absolute children below
        // are laid out relative to the playback view.
        let video_bounds = local_video_viewport_bounds(observed_video_bounds);
        let Some(source_size) = self.frame.source_size else {
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

        let bitmap_canvas_size = subtitle_bitmap_canvas_size(cue).unwrap_or(source_size);
        let bitmap_bounds =
            aspect_fit_bounds(video_bounds, bitmap_canvas_size).unwrap_or(video_fitted_bounds);
        let scale_x = bitmap_bounds.size.width / px(bitmap_canvas_size.width as f32);
        let scale_y = bitmap_bounds.size.height / px(bitmap_canvas_size.height as f32);
        let subtitle_bottom =
            subtitle_overlay_bottom(video_fitted_bounds, video_bounds, progress_bar_visible);
        let subtitle_render_bottom = subtitle_render_bottom(
            video_fitted_bounds,
            subtitle_bottom,
            self.subtitle.vertical_offset_fraction,
        );
        let bitmap_top = if self.subtitle.vertical_offset_fraction.is_some() {
            subtitle_bitmap_overlay_top_for_bottom(
                cue,
                bitmap_bounds,
                scale_y,
                subtitle_render_bottom,
            )
        } else {
            let bitmap_bottom_offset =
                subtitle_bitmap_bottom_offset(cue, bitmap_bounds, scale_y, subtitle_bottom);
            subtitle_bitmap_overlay_top(bitmap_bounds, bitmap_bottom_offset)
        };
        let bitmap_overlay = cue.bitmaps.iter().fold(
            div()
                .id("playback-subtitle-bitmap-overlay")
                .absolute()
                .left(bitmap_bounds.origin.x)
                .top(bitmap_top)
                .w(bitmap_bounds.size.width)
                .h(bitmap_bounds.size.height),
            |this, bitmap| this.child(render_subtitle_bitmap(bitmap, scale_x, scale_y)),
        );
        let overlay = div()
            .id("playback-subtitle-overlay")
            .absolute()
            .left_0()
            .top_0()
            .w_full()
            .h_full()
            .child(bitmap_overlay);

        if cue.text.trim().is_empty() {
            return overlay.into_any_element();
        }

        let text_overlay_bounds = subtitle_text_overlay_bounds(
            video_fitted_bounds,
            subtitle_render_bottom,
            self.subtitle.vertical_offset_fraction,
        );
        overlay
            .child(
                div()
                    .id("playback-subtitle-text-overlay")
                    .absolute()
                    .left(text_overlay_bounds.origin.x)
                    .top(text_overlay_bounds.origin.y)
                    .w(text_overlay_bounds.size.width)
                    .h(text_overlay_bounds.size.height)
                    .flex()
                    .justify_center()
                    .items_end()
                    .px_6()
                    .child(
                        div()
                            .max_w(relative(0.86))
                            .px_3()
                            .text_center()
                            .text_3xl()
                            .line_height(px(36.0))
                            .text_color(rgb(0xffffff))
                            .child(cue.text.clone()),
                    ),
            )
            .into_any_element()
    }
}

pub(super) fn render_subtitle_bitmap(
    bitmap: &BackendSubtitleBitmap,
    scale_x: f32,
    scale_y: f32,
) -> impl IntoElement {
    div()
        .absolute()
        .left(px(bitmap.x as f32) * scale_x)
        .top(px(bitmap.y as f32) * scale_y)
        .w(px(bitmap.width as f32) * scale_x)
        .h(px(bitmap.height as f32) * scale_y)
        .child(SubtitleBitmapElement {
            image: bitmap.image.clone(),
        })
}

pub(super) fn local_video_viewport_bounds(bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds::new(gpui::point(px(0.0), px(0.0)), bounds.size)
}

pub(super) fn subtitle_overlay_bottom(
    video_fitted_bounds: Bounds<Pixels>,
    video_bounds: Bounds<Pixels>,
    progress_bar_visible: bool,
) -> Pixels {
    let video_bottom = subtitle_video_bottom(video_fitted_bounds);
    if progress_bar_visible {
        let controls_top = playback_progress_bar_bounds(video_bounds).origin.y;
        video_bottom.min(controls_top)
    } else {
        video_bottom
    }
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

pub(super) fn subtitle_bitmap_overlay_top_for_bottom(
    cue: &BackendSubtitleCue,
    bitmap_bounds: Bounds<Pixels>,
    scale_y: f32,
    subtitle_bottom: Pixels,
) -> Pixels {
    let Some(content_bottom) = subtitle_bitmap_content_bottom(cue, bitmap_bounds, scale_y) else {
        return bitmap_bounds.origin.y;
    };

    bitmap_bounds.origin.y - (content_bottom - subtitle_bottom)
}

pub(super) fn subtitle_vertical_offset_after_adjustment(current_offset: f32, delta: f32) -> f32 {
    current_offset + delta
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

pub(super) fn subtitle_vertical_offset_fraction(
    video_fitted_bounds: Bounds<Pixels>,
    offset: Pixels,
) -> f32 {
    let video_height = f32::from(video_fitted_bounds.size.height);
    if video_height > 0.0 {
        f32::from(offset) / video_height
    } else {
        0.0
    }
}

#[cfg(test)]
pub(super) fn subtitle_text_overlay_height(
    video_fitted_bounds: Bounds<Pixels>,
    video_bounds: Bounds<Pixels>,
    progress_bar_visible: bool,
) -> Pixels {
    let bottom = subtitle_overlay_bottom(video_fitted_bounds, video_bounds, progress_bar_visible);
    subtitle_text_overlay_height_for_bottom(video_fitted_bounds, bottom)
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
    cue.bitmaps.iter().fold(None, |bottom, bitmap| {
        let bitmap_bottom =
            bitmap_bounds.origin.y + px(bitmap.y.saturating_add(bitmap.height) as f32) * scale_y;
        Some(bottom.map_or(bitmap_bottom, |bottom: Pixels| bottom.max(bitmap_bottom)))
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

pub(super) fn defer_drop_subtitle(cue: Option<BackendSubtitleCue>, window: &mut Window) {
    if let Some(cue) = cue {
        for bitmap in cue.bitmaps {
            defer_drop_frame(bitmap.image, window);
        }
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

    use crate::player::{
        backend::{BackendSubtitleBitmap, BackendSubtitleCue},
        render_host::{RenderSize, render_image_from_bgra},
    };

    use super::*;

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
    fn subtitle_text_overlay_height_stops_at_progress_bar_top() {
        let video_bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(600.0)));
        let video_fitted_bounds = Bounds::new(point(px(0.0), px(75.0)), size(px(800.0), px(450.0)));

        assert_eq!(
            subtitle_text_overlay_height(video_fitted_bounds, video_bounds, true),
            px(407.0)
        );
    }

    #[test]
    fn subtitle_text_overlay_height_uses_video_bounds_origin_for_controls_top() {
        let video_bounds = Bounds::new(point(px(10.0), px(20.0)), size(px(800.0), px(600.0)));
        let video_fitted_bounds =
            Bounds::new(point(px(10.0), px(95.0)), size(px(800.0), px(450.0)));

        assert_eq!(
            subtitle_text_overlay_height(video_fitted_bounds, video_bounds, true),
            px(407.0)
        );
    }

    #[test]
    fn subtitle_text_overlay_height_uses_video_bottom_without_visible_progress_bar() {
        let video_bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(700.0)));
        let video_fitted_bounds = Bounds::new(point(px(0.0), px(75.0)), size(px(800.0), px(450.0)));

        assert_eq!(
            subtitle_text_overlay_height(video_fitted_bounds, video_bounds, true),
            px(450.0)
        );
        assert_eq!(
            subtitle_text_overlay_height(video_fitted_bounds, video_bounds, false),
            px(450.0)
        );
    }

    #[test]
    fn subtitle_text_overlay_offset_shifts_position_without_changing_default_height() {
        let video_fitted_bounds = Bounds::new(point(px(0.0), px(75.0)), size(px(800.0), px(450.0)));

        let raised = subtitle_text_overlay_bounds(video_fitted_bounds, px(25.0), Some(1.0));
        assert_eq!(raised.origin.y, px(-425.0));
        assert_eq!(raised.size.height, px(450.0));

        let lowered = subtitle_text_overlay_bounds(video_fitted_bounds, px(530.0), Some(-0.01));
        assert_eq!(lowered.origin.y, px(80.0));
        assert_eq!(lowered.size.height, px(450.0));
    }

    #[test]
    fn subtitle_render_bottom_offset_is_independent_from_controls_visibility() {
        let video_bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(600.0)));
        let video_fitted_bounds = Bounds::new(point(px(0.0), px(75.0)), size(px(800.0), px(450.0)));
        let controls_default_bottom = px(482.0);
        let hidden_controls_default_bottom = px(525.0);

        let manual_offset = subtitle_vertical_offset_after_adjustment(
            subtitle_vertical_offset_fraction(
                video_fitted_bounds,
                px(525.0) - controls_default_bottom,
            ),
            subtitle_vertical_adjust_step(),
        );

        assert_eq!(
            subtitle_render_bottom(
                video_fitted_bounds,
                subtitle_text_overlay_height(video_fitted_bounds, video_bounds, true)
                    + video_fitted_bounds.origin.y,
                Some(manual_offset),
            ),
            px(477.5)
        );
        assert_eq!(
            subtitle_render_bottom(
                video_fitted_bounds,
                hidden_controls_default_bottom,
                Some(manual_offset),
            ),
            px(477.5)
        );
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
    fn subtitle_bitmap_overlay_offset_shifts_position_without_changing_default_limit() {
        let bitmap_bounds = Bounds::new(point(px(0.0), px(75.0)), size(px(800.0), px(450.0)));
        let image = render_image_from_bgra(vec![0, 0, 0, 0], 1, 1).unwrap();
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

        assert_eq!(
            subtitle_bitmap_overlay_top_for_bottom(&cue, bitmap_bounds, 1.0, px(575.0)),
            px(-425.0)
        );
        assert_eq!(
            subtitle_bitmap_overlay_top_for_bottom(&cue, bitmap_bounds, 1.0, px(1035.0)),
            px(35.0)
        );
    }

    #[test]
    fn subtitle_vertical_offset_adjustment_allows_moving_past_default_position() {
        assert_eq!(subtitle_vertical_offset_after_adjustment(0.0, -0.01), -0.01);
        assert_eq!(
            subtitle_vertical_offset_after_adjustment(0.02, -0.03),
            -0.01
        );
        assert_eq!(subtitle_vertical_offset_after_adjustment(0.02, 0.01), 0.03);
    }

    #[test]
    fn subtitle_bitmap_bottom_offset_lifts_only_overlapping_bitmap_content() {
        let image = render_image_from_bgra(vec![0, 0, 0, 0], 1, 1).unwrap();
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
        let image = render_image_from_bgra(vec![0, 0, 0, 0], 1, 1).unwrap();
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
}
