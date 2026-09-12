use super::*;

use gpui::{Animation, AnimationExt as _, Transformation, percentage};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PlaybackStatus {
    Loading,
    Error(SharedString),
}

pub(super) fn playback_status(
    timeline: &PlaybackTimelineState,
    has_visible_frame: bool,
    switching_episode: bool,
    error: Option<&SharedString>,
) -> Option<PlaybackStatus> {
    if let Some(error) = error {
        return Some(PlaybackStatus::Error(error.clone()));
    }

    let waiting_for_seek =
        timeline.pending_seek_position.is_some() && !timeline.pending_seek_keeps_frame;
    let waiting_for_cache = timeline.paused_for_cache && !timeline.user_paused;
    (switching_episode
        || (!timeline.ended
            && (timeline.buffering || waiting_for_seek || waiting_for_cache || !has_visible_frame)))
        .then_some(PlaybackStatus::Loading)
}

pub(super) fn playback_loader(id: &'static str, size: Pixels, cx: &gpui::App) -> impl IntoElement {
    svg()
        .debug_selector(move || id.to_string())
        .path("icons/loader.svg")
        .size(size)
        .flex_none()
        .overflow_hidden()
        .text_color(theme::get(cx).accent)
        .with_animation(
            id,
            Animation::new(Duration::from_millis(1800)).repeat(),
            |svg, delta| svg.with_transformation(Transformation::rotate(percentage(delta))),
        )
}

pub(super) fn render_playback_status(status: PlaybackStatus, cx: &gpui::App) -> impl IntoElement {
    let content = match status {
        PlaybackStatus::Loading => {
            playback_loader("playback-loader", px(24.0), cx).into_any_element()
        }
        PlaybackStatus::Error(message) => div()
            .debug_selector(|| "playback-error-message".to_string())
            .child(message)
            .into_any_element(),
    };

    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_base()
        .text_color(theme::media_overlay(cx).muted_foreground)
        .child(content)
}

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
    pub(super) video_presenter_needs_frame: bool,
}

pub(super) fn should_request_animation_frame(state: AnimationFrameRequestState) -> bool {
    state.has_backend
        && state.has_video_presenter
        && !state.has_error
        && !state.playback_ended
        && (!state.has_loaded_file
            || state.playback_buffering
            || state.pending_seek
            || (state.has_viewport
                && (state.video_presenter_needs_frame
                    || !state.playback_paused
                    || !state.has_visible_frame)))
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, TestAppContext, point, px, size};

    use crate::player::render_host::RenderSize;

    use super::*;

    #[test]
    fn playback_loader_tracks_first_frame_buffering_and_episode_switches() {
        let mut timeline = PlaybackTimelineState::default();
        assert_eq!(
            playback_status(&timeline, false, false, None),
            Some(PlaybackStatus::Loading)
        );
        timeline.loaded = true;
        assert_eq!(
            playback_status(&timeline, false, false, None),
            Some(PlaybackStatus::Loading)
        );
        assert_eq!(playback_status(&timeline, true, false, None), None);

        timeline.buffering = true;
        assert_eq!(
            playback_status(&timeline, true, false, None),
            Some(PlaybackStatus::Loading)
        );
        timeline.buffering = false;
        timeline.ended = true;
        assert_eq!(playback_status(&timeline, false, false, None), None);
        assert_eq!(
            playback_status(&timeline, true, true, None),
            Some(PlaybackStatus::Loading)
        );

        let error = SharedString::from("加载视频失败：连接断开");
        assert_eq!(
            playback_status(&timeline, false, true, Some(&error)),
            Some(PlaybackStatus::Error(error))
        );
    }

    #[test]
    fn playback_loader_waits_for_uncached_seek_but_keeps_cached_seeks_unobstructed() {
        let mut timeline = PlaybackTimelineState {
            loaded: true,
            pending_seek_position: Some(120.0),
            ..PlaybackTimelineState::default()
        };
        assert_eq!(
            playback_status(&timeline, true, false, None),
            Some(PlaybackStatus::Loading)
        );

        timeline.pending_seek_keeps_frame = true;
        assert_eq!(playback_status(&timeline, true, false, None), None);
    }

    #[test]
    fn playback_loader_shows_cache_stalls_without_treating_user_pause_as_loading() {
        let mut timeline = PlaybackTimelineState {
            loaded: true,
            paused_for_cache: true,
            user_paused: false,
            ..PlaybackTimelineState::default()
        };
        assert_eq!(
            playback_status(&timeline, true, false, None),
            Some(PlaybackStatus::Loading)
        );

        timeline.user_paused = true;
        assert_eq!(playback_status(&timeline, true, false, None), None);
        timeline.user_paused = false;
        timeline.paused_for_cache = false;
        assert_eq!(playback_status(&timeline, true, false, None), None);
    }

    #[gpui::test]
    fn playback_status_renders_centered_loader_and_replaces_it_with_errors(
        cx: &mut TestAppContext,
    ) {
        struct StatusPreview {
            status: PlaybackStatus,
            has_video_frame: bool,
        }

        impl Render for StatusPreview {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div()
                    .relative()
                    .size_full()
                    .overflow_hidden()
                    // Match VideoFrameElement's full-size layout while seeking or switching tracks.
                    .when(self.has_video_frame, |this| {
                        this.child(div().size_full().bg(rgb(0x000000)))
                    })
                    .child(render_playback_status(self.status.clone(), cx))
            }
        }

        cx.update(theme::init);
        let (view, cx) = cx.add_window_view(|_, _| StatusPreview {
            status: PlaybackStatus::Loading,
            has_video_frame: false,
        });
        for selection in theme::ColorTheme::ALL {
            cx.update(|_, cx| theme::set(selection, cx));
            for has_video_frame in [false, true] {
                view.update(cx, |view, cx| {
                    view.has_video_frame = has_video_frame;
                    cx.notify();
                });
                for (width, height) in [(640.0, 360.0), (1280.0, 720.0)] {
                    cx.simulate_resize(size(px(width), px(height)));
                    cx.run_until_parked();
                    let loader = cx.debug_bounds("playback-loader").unwrap();
                    assert_eq!(loader.size, size(px(24.0), px(24.0)));
                    assert_eq!(loader.center(), point(px(width / 2.0), px(height / 2.0)));
                    assert!(cx.debug_bounds("playback-error-message").is_none());
                }
            }
        }

        view.update(cx, |view, cx| {
            view.status = PlaybackStatus::Error("加载视频失败：连接断开".into());
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("playback-loader").is_none());
        assert_eq!(
            cx.debug_bounds("playback-error-message").unwrap().center(),
            point(px(640.0), px(360.0))
        );
    }

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

    #[test]
    fn should_request_animation_frame_continues_while_presenter_has_work() {
        assert!(should_request_animation_frame(AnimationFrameRequestState {
            video_presenter_needs_frame: true,
            ..animation_frame_request_state()
        }));
    }

    #[test]
    fn should_request_animation_frame_waits_for_viewport_when_only_presenter_has_work() {
        assert!(!should_request_animation_frame(
            AnimationFrameRequestState {
                has_viewport: false,
                video_presenter_needs_frame: true,
                ..animation_frame_request_state()
            }
        ));
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
            video_presenter_needs_frame: false,
        }
    }
}
