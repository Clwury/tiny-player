use super::*;

pub(super) fn normalize_video_viewport(bounds: Bounds<Pixels>) -> Option<(u32, u32)> {
    let width = f32::from(bounds.size.width).floor().max(0.0) as u32;
    let height = f32::from(bounds.size.height).floor().max(0.0) as u32;

    (width > 0 && height > 0).then_some((width, height))
}

pub(super) fn aspect_fit_bounds(
    bounds: Bounds<Pixels>,
    source: RenderSize,
) -> Option<Bounds<Pixels>> {
    if source.width == 0 || source.height == 0 {
        return None;
    }

    let container_width = f32::from(bounds.size.width).max(0.0);
    let container_height = f32::from(bounds.size.height).max(0.0);
    if container_width == 0.0 || container_height == 0.0 {
        return None;
    }

    let source_width = source.width as f32;
    let source_height = source.height as f32;
    let scale = (container_width / source_width).min(container_height / source_height);
    let fitted_width = source_width * scale;
    let fitted_height = source_height * scale;
    let inset_x = (container_width - fitted_width) / 2.0;
    let inset_y = (container_height - fitted_height) / 2.0;

    Some(Bounds::new(
        gpui::point(bounds.origin.x + px(inset_x), bounds.origin.y + px(inset_y)),
        gpui::size(px(fitted_width), px(fitted_height)),
    ))
}

pub(super) fn render_output_size(bounds: Bounds<Pixels>, source: RenderSize) -> Option<RenderSize> {
    let (width, height) = normalize_video_viewport(aspect_fit_bounds(bounds, source)?)?;
    Some(RenderSize {
        width: width.min(source.width),
        height: height.min(source.height),
    })
}

pub(super) fn defer_drop_frame(frame: Arc<RenderImage>, window: &mut Window) {
    window.on_next_frame(move |window, _| {
        window.on_next_frame(move |window, cx| {
            cx.drop_image(frame, Some(window));
        });
        window.refresh();
    });
    window.refresh();
}

pub(super) fn viewport_changed(previous: Option<Bounds<Pixels>>, next: Bounds<Pixels>) -> bool {
    previous != Some(next)
}

pub(super) fn should_render_frame(
    has_video_presenter: bool,
    has_loaded_file: bool,
    has_error: bool,
    has_video_size: bool,
    has_viewport: bool,
) -> bool {
    has_video_presenter && has_loaded_file && !has_error && has_video_size && has_viewport
}

#[derive(Clone, Copy)]
pub(super) struct AnimationFrameRequestState {
    pub(super) has_backend: bool,
    pub(super) has_video_presenter: bool,
    pub(super) has_loaded_file: bool,
    pub(super) has_error: bool,
    pub(super) has_viewport: bool,
    pub(super) has_visible_frame: bool,
    pub(super) playback_paused: bool,
    pub(super) playback_buffering: bool,
    pub(super) pending_seek: bool,
}

pub(super) fn should_request_animation_frame(state: AnimationFrameRequestState) -> bool {
    state.has_backend
        && state.has_video_presenter
        && !state.has_error
        && (!state.has_loaded_file
            || state.playback_buffering
            || state.pending_seek
            || (state.has_viewport && (!state.playback_paused || !state.has_visible_frame)))
}
