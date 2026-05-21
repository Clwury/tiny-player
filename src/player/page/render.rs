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
    pub(super) playback_ended: bool,
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
        && !state.playback_ended
        && (!state.has_loaded_file
            || state.playback_buffering
            || state.pending_seek
            || (state.has_viewport && (!state.playback_paused || !state.has_visible_frame)))
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, point, px, size};

    use crate::player::render_host::RenderSize;

    use super::*;

    #[test]
    fn normalize_video_viewport_rejects_zero_sized_bounds() {
        let zero_width = Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(180.0)));
        let zero_height = Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(0.0)));

        assert_eq!(normalize_video_viewport(zero_width), None);
        assert_eq!(normalize_video_viewport(zero_height), None);
    }

    #[test]
    fn normalize_video_viewport_floors_fractional_pixel_sizes() {
        let bounds = Bounds::new(point(px(10.0), px(12.0)), size(px(640.8), px(359.9)));

        assert_eq!(normalize_video_viewport(bounds), Some((640, 359)));
    }

    #[test]
    fn viewport_changed_only_reports_real_differences() {
        let first = Bounds::new(point(px(0.0), px(0.0)), size(px(320.0), px(240.0)));
        let second = Bounds::new(point(px(0.0), px(0.0)), size(px(400.0), px(240.0)));

        assert!(viewport_changed(None, first));
        assert!(!viewport_changed(Some(first), first));
        assert!(viewport_changed(Some(first), second));
    }

    #[test]
    fn aspect_fit_bounds_letterboxes_wide_video_in_tall_viewport() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(600.0)));
        let fitted = aspect_fit_bounds(
            bounds,
            RenderSize {
                width: 1920,
                height: 1080,
            },
        )
        .unwrap();

        assert_eq!(fitted.origin, point(px(0.0), px(75.0)));
        assert_eq!(fitted.size, size(px(800.0), px(450.0)));
    }

    #[test]
    fn aspect_fit_bounds_pillarboxes_tall_video_in_wide_viewport() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(1280.0), px(720.0)));
        let fitted = aspect_fit_bounds(
            bounds,
            RenderSize {
                width: 640,
                height: 480,
            },
        )
        .unwrap();

        assert_eq!(fitted.origin, point(px(160.0), px(0.0)));
        assert_eq!(fitted.size, size(px(960.0), px(720.0)));
    }

    #[test]
    fn aspect_fit_bounds_rejects_zero_source_or_viewport() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(800.0), px(600.0)));
        let zero_bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(600.0)));

        assert_eq!(
            aspect_fit_bounds(
                bounds,
                RenderSize {
                    width: 0,
                    height: 1080,
                },
            ),
            None
        );
        assert_eq!(
            aspect_fit_bounds(
                zero_bounds,
                RenderSize {
                    width: 1920,
                    height: 1080,
                },
            ),
            None
        );
    }

    #[test]
    fn aspect_fit_bounds_handles_fractional_viewport_sizes() {
        let bounds = Bounds::new(point(px(10.0), px(12.0)), size(px(640.8), px(359.9)));
        let fitted = aspect_fit_bounds(
            bounds,
            RenderSize {
                width: 3840,
                height: 2160,
            },
        )
        .unwrap();

        assert_eq!(normalize_video_viewport(fitted), Some((639, 359)));
    }

    #[test]
    fn render_output_size_uses_aspect_fitted_viewport() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(1920.0), px(1080.0)));

        assert_eq!(
            render_output_size(
                bounds,
                RenderSize {
                    width: 3840,
                    height: 1600,
                },
            ),
            Some(RenderSize {
                width: 1920,
                height: 800,
            })
        );
    }

    #[test]
    fn render_output_size_does_not_upscale_past_source() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(3840.0), px(2160.0)));

        assert_eq!(
            render_output_size(
                bounds,
                RenderSize {
                    width: 1280,
                    height: 720,
                },
            ),
            Some(RenderSize {
                width: 1280,
                height: 720,
            })
        );
    }

    #[test]
    fn should_render_frame_requires_loaded_file_video_size_and_valid_viewport() {
        assert!(should_render_frame(true, true, false, true, true));
        assert!(!should_render_frame(false, true, false, true, true));
        assert!(!should_render_frame(true, false, false, true, true));
        assert!(!should_render_frame(true, true, true, true, true));
        assert!(!should_render_frame(true, true, false, false, true));
        assert!(!should_render_frame(true, true, false, true, false));
    }

    #[test]
    fn should_request_animation_frame_drives_initial_load() {
        assert!(should_request_animation_frame(AnimationFrameRequestState {
            has_loaded_file: false,
            has_viewport: false,
            playback_paused: true,
            ..animation_frame_request_state()
        }));
    }

    #[test]
    fn should_request_animation_frame_requires_backend_and_presenter() {
        assert!(!should_request_animation_frame(
            AnimationFrameRequestState {
                has_backend: false,
                has_loaded_file: false,
                has_viewport: false,
                playback_paused: true,
                ..animation_frame_request_state()
            }
        ));
        assert!(!should_request_animation_frame(
            AnimationFrameRequestState {
                has_video_presenter: false,
                has_loaded_file: false,
                has_viewport: false,
                playback_paused: true,
                ..animation_frame_request_state()
            }
        ));
    }

    #[test]
    fn should_request_animation_frame_stops_on_error() {
        assert!(!should_request_animation_frame(
            AnimationFrameRequestState {
                has_loaded_file: false,
                has_error: true,
                has_viewport: false,
                playback_paused: true,
                ..animation_frame_request_state()
            }
        ));
    }

    #[test]
    fn should_request_animation_frame_stops_after_playback_ends() {
        assert!(!should_request_animation_frame(
            AnimationFrameRequestState {
                has_visible_frame: false,
                playback_ended: true,
                ..animation_frame_request_state()
            }
        ));
    }

    #[test]
    fn should_request_animation_frame_requires_unpaused_loaded_video_with_viewport() {
        assert!(should_request_animation_frame(AnimationFrameRequestState {
            playback_paused: false,
            ..animation_frame_request_state()
        }));
        assert!(!should_request_animation_frame(
            animation_frame_request_state()
        ));
        assert!(!should_request_animation_frame(
            AnimationFrameRequestState {
                has_viewport: false,
                playback_paused: false,
                ..animation_frame_request_state()
            }
        ));
    }

    #[test]
    fn should_request_animation_frame_continues_until_first_visible_frame() {
        assert!(should_request_animation_frame(AnimationFrameRequestState {
            has_visible_frame: false,
            ..animation_frame_request_state()
        }));
    }

    #[test]
    fn should_request_animation_frame_continues_while_buffering() {
        assert!(should_request_animation_frame(AnimationFrameRequestState {
            has_viewport: false,
            playback_buffering: true,
            ..animation_frame_request_state()
        }));
    }

    #[test]
    fn should_request_animation_frame_continues_during_soft_seek() {
        assert!(should_request_animation_frame(AnimationFrameRequestState {
            has_viewport: false,
            pending_seek: true,
            ..animation_frame_request_state()
        }));
    }

    fn animation_frame_request_state() -> AnimationFrameRequestState {
        AnimationFrameRequestState {
            has_backend: true,
            has_video_presenter: true,
            has_loaded_file: true,
            playback_ended: false,
            has_error: false,
            has_viewport: true,
            has_visible_frame: true,
            playback_paused: true,
            playback_buffering: false,
            pending_seek: false,
        }
    }
}
