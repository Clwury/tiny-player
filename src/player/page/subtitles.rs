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
        self.subtitle_vertical_offset_fraction = Some(subtitle_vertical_offset_after_adjustment(
            current_offset_fraction,
            delta,
        ));
        cx.notify();
    }

    pub(super) fn current_subtitle_vertical_offset_fraction(&self, window: &Window) -> f32 {
        self.subtitle_vertical_offset_fraction
            .or_else(|| self.default_subtitle_vertical_offset_fraction(window))
            .unwrap_or(0.0)
    }

    pub(super) fn default_subtitle_vertical_offset_fraction(&self, window: &Window) -> Option<f32> {
        let (video_bounds, video_fitted_bounds) = self.current_video_layout_bounds()?;
        let default_bottom = subtitle_overlay_bottom(
            video_fitted_bounds,
            video_bounds,
            self.progress_bar_visible(window.is_fullscreen()),
        );

        Some(subtitle_vertical_offset_fraction(
            video_fitted_bounds,
            subtitle_video_bottom(video_fitted_bounds) - default_bottom,
        ))
    }

    pub(super) fn current_video_layout_bounds(&self) -> Option<(Bounds<Pixels>, Bounds<Pixels>)> {
        let video_bounds = local_video_viewport_bounds(self.video_viewport_bounds?);
        let video_fitted_bounds = aspect_fit_bounds(video_bounds, self.video_source_size?)?;
        Some((video_bounds, video_fitted_bounds))
    }

    pub(super) fn render_subtitle_overlay(
        &self,
        progress_bar_visible: bool,
        _cx: &Context<Self>,
    ) -> impl IntoElement {
        let Some(cue) = self.active_subtitle.as_ref() else {
            return div()
                .id("playback-subtitle-overlay-empty")
                .into_any_element();
        };
        let Some(observed_video_bounds) = self.video_viewport_bounds else {
            return div()
                .id("playback-subtitle-overlay-empty")
                .into_any_element();
        };
        // Canvas observations are window-relative, while absolute children below
        // are laid out relative to the playback view.
        let video_bounds = local_video_viewport_bounds(observed_video_bounds);
        let Some(source_size) = self.video_source_size else {
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
            self.subtitle_vertical_offset_fraction,
        );
        let bitmap_top = if self.subtitle_vertical_offset_fraction.is_some() {
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
            self.subtitle_vertical_offset_fraction,
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
