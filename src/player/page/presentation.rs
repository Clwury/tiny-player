//! Page-owned GPUI presentation resources. Layout/input and accepted session
//! effects are the only writers. Back cancels deadlines and retires visible
//! images; entity release cancels remaining handles and retires images after
//! old scenes have completed their two frame callbacks.
use super::*;

pub(super) struct PlaybackPresentationState {
    pub(super) focus_handle: FocusHandle,
    pub(super) frame: PlaybackFrameState,
    pub(super) timeline_presentation: PlaybackTimelinePresentation,
    pub(super) presentation_timers: PresentationEffects,
    pub(super) download_speed: controls::DownloadSpeedDisplay,
    pub(super) playback_details_visible: bool,
    pub(super) fullscreen: FullscreenControlsState,
    pub(super) window_drag: WindowDragState,
    pub(super) episode_list: episodes::PlaybackEpisodeListState,
    pub(super) track_select_open: Option<PlaybackTrackKind>,
    pub(super) subtitle: SubtitleOverlayState,
    pub(super) volume_indicator_visible: bool,
    pub(super) rate_indicator_visible: bool,
}

impl PlaybackPresentationState {
    pub(super) fn new(
        focus_handle: FocusHandle,
        identity: crate::effects::WorkspaceIdentity,
        episode_list: episodes::PlaybackEpisodeListState,
    ) -> Self {
        Self {
            focus_handle,
            frame: Default::default(),
            timeline_presentation: Default::default(),
            presentation_timers: PresentationEffects::new(identity),
            download_speed: Default::default(),
            playback_details_visible: false,
            fullscreen: Default::default(),
            window_drag: Default::default(),
            episode_list,
            track_select_open: None,
            subtitle: Default::default(),
            volume_indicator_visible: false,
            rate_indicator_visible: false,
        }
    }

    /// Take each image once for the existing deferred atlas eviction path.
    pub(super) fn release_images(&mut self) -> Vec<Arc<RenderImage>> {
        self.presentation_timers.close();
        let mut images = self.subtitle.images.update(None);
        images.extend(self.frame.current.take());
        images
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum WindowDragState {
    #[default]
    Idle,
    Pending,
    Blocked,
}

#[derive(Default)]
pub(super) struct PlaybackFrameState {
    pub(super) viewport_bounds: Option<Bounds<Pixels>>,
    pub(super) source_size: Option<RenderSize>,
    pub(super) current: Option<Arc<RenderImage>>,
}

/// Page-owned hit-test bounds, hover and cache popover. Layout/input write these;
/// restart/close invalidates the popover, fullscreen clears hover, release drops all.
#[derive(Default)]
pub(super) struct PlaybackTimelinePresentation {
    pub(super) cache_status_open: bool,
    pub(super) progress_track_bounds: Option<Bounds<Pixels>>,
    pub(super) progress_hover_cursor: Option<Point<Pixels>>,
}

#[derive(Default)]
pub(super) struct FullscreenControlsState {
    pub(super) cursor_visible: bool,
    pub(super) controls_visible: bool,
    pub(super) mouse_in_controls: bool,
    pub(super) mouse_in_back_button: bool,
}

#[derive(Default)]
pub(super) struct SubtitleOverlayState {
    pub(super) active: Option<BackendSubtitleCue>,
    pub(super) images: SubtitleImages,
    pub(super) vertical_offset_fraction: Option<f32>,
}

impl PlaybackPage {
    pub(super) fn replace_visible_frame(
        &mut self,
        frame: Arc<RenderImage>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        if self
            .presentation
            .frame
            .current
            .as_ref()
            .is_some_and(|current| current.id == frame.id)
        {
            self.presentation.frame.current = Some(frame);
            return;
        }

        let previous = self.presentation.frame.current.replace(frame);
        if let Some(previous) = previous {
            defer_drop_frame(previous, window);
        }
    }

    pub(super) fn clear_visible_frame(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        if let Some(frame) = self.presentation.frame.current.take() {
            defer_drop_frame(frame, window);
        }
    }

    pub(super) fn update_video_viewport(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        if !viewport_changed(self.presentation.frame.viewport_bounds, bounds) {
            return;
        }

        self.presentation.frame.viewport_bounds = Some(bounds);
        cx.notify();
    }

    pub(super) fn update_progress_track_bounds(
        &mut self,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if !viewport_changed(
            self.presentation
                .timeline_presentation
                .progress_track_bounds,
            bounds,
        ) {
            return;
        }

        self.presentation
            .timeline_presentation
            .progress_track_bounds = Some(bounds);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_playback::{BgraImage, SharedBgraImage};

    struct Empty;

    impl Render for Empty {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full()
        }
    }

    #[gpui::test]
    fn replacing_page_retires_video_and_subtitle_images_after_two_frames(
        cx: &mut gpui::TestAppContext,
    ) {
        let (page, cx) = test_support::playback_window(cx);
        let video = render_image(BgraImage::new(vec![1, 2, 3, 255], 1, 1).unwrap());
        let bitmap = SharedBgraImage::new(BgraImage::new(vec![4, 5, 6, 255], 1, 1).unwrap());
        let subtitle = page.update(cx, |page, cx| {
            page.video = crate::player::backend::test_support::adapter(Default::default(), true);
            page.session.timeline_mut().paused = true;
            let cue = BackendSubtitleCue {
                text: String::new(),
                bitmaps: vec![BackendSubtitleBitmap {
                    image: bitmap.clone(),
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                    canvas_width: 1,
                    canvas_height: 1,
                }],
                start_nsecs: 0,
                end_nsecs: 1_000_000_000,
            };
            page.presentation.frame.current = Some(video.clone());
            page.presentation.frame.source_size = Some(RenderSize {
                width: 1,
                height: 1,
            });
            page.presentation.frame.viewport_bounds = Some(Bounds::new(
                gpui::point(px(0.0), px(0.0)),
                gpui::size(px(800.0), px(800.0)),
            ));
            page.presentation.subtitle.images.update(Some(&cue));
            page.presentation.subtitle.active = Some(cue);
            page.show_volume_indicator(cx);
            page.presentation
                .subtitle
                .images
                .get(&bitmap)
                .unwrap()
                .clone()
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(window.has_image_atlas_entry(&video));
            assert!(window.has_image_atlas_entry(&subtitle));
            window.replace_root(cx, |_, _| Empty);
        });
        let old = page.downgrade();
        drop(page);
        cx.run_until_parked();
        cx.update(|_, _| {});
        cx.update(|window, cx| {
            assert!(old.upgrade().is_none());
            for image in [&video, &subtitle] {
                assert!(window.has_image_atlas_entry(image));
            }
            window.simulate_next_frame(cx);
            for image in [&video, &subtitle] {
                assert!(window.has_image_atlas_entry(image));
            }
            window.simulate_next_frame(cx);
            for image in [&video, &subtitle] {
                assert!(!window.has_image_atlas_entry(image));
            }
        });
    }
}
