//! Construction, teardown registration and app-facing navigation.
use super::*;

impl PlaybackPage {
    pub(crate) fn apply_playback_config(
        &mut self,
        config: tiny_playback::PlaybackCacheConfig,
    ) -> tiny_playback::Result<()> {
        if let Some(result) = self.video.command(BackendCommand::SetCacheConfig(config)) {
            result?;
        }
        Ok(())
    }

    pub fn new(request: PlaybackRequest, cx: &mut Context<Self>) -> Self {
        Self::new_with_cache_config(request, Default::default(), cx)
    }

    pub fn new_with_cache_config(
        request: PlaybackRequest,
        cache_config: tiny_playback::PlaybackCacheConfig,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_settings(request, cache_config, PlaybackVolumeSettings::default(), cx)
    }

    pub(crate) fn new_with_settings(
        request: PlaybackRequest,
        cache_config: tiny_playback::PlaybackCacheConfig,
        volume_settings: PlaybackVolumeSettings,
        cx: &mut Context<Self>,
    ) -> Self {
        let ports = crate::player::adapter::playback_ports(&request.emby);
        Self::with_ports(request, cache_config, volume_settings, ports, cx)
    }

    pub(crate) fn with_ports(
        request: PlaybackRequest,
        cache_config: tiny_playback::PlaybackCacheConfig,
        volume_settings: PlaybackVolumeSettings,
        ports: crate::player::PlaybackPorts,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::register_image_cleanup(cx);
        let volume = volume_settings.normalized();
        let source_protocol = playback_protocol(&request.url);
        let content_length = request.content_length;
        let (report_effects, queue_effects) = bind_ports(&request.emby, ports, cx);

        let (video, error_message) = PlaybackBackendAdapter::start(
            BackendLoadRequest {
                url: request.url.clone(),
                http_headers: request.http_headers.clone(),
                content_length: request.content_length,
                start_position_seconds: request.initial_position_seconds,
                selected_tracks: request.selected_tracks.clone(),
                cache_config,
            },
            volume.level,
        );

        let timeline = PlaybackTimelineState {
            position: valid_playback_time(request.initial_position_seconds),
            ..PlaybackTimelineState::default()
        };

        let mut page = Self {
            title: request.title,
            video,
            presentation: PlaybackPresentationState::new(
                cx.focus_handle(),
                request.emby.server.workspace_identity(),
                episodes::PlaybackEpisodeListState::new(&request.emby),
            ),
            session: crate::player::session::PlaybackSessionController::new(
                timeline,
                PlaybackSourceState {
                    source_protocol,
                    source_url: request.url,
                    content_length,
                    playback_file_info: None,
                    playback_info: None,
                    playback_audio_info: None,
                    tracks: PlaybackTrackState::new(
                        request.audio_tracks,
                        request.subtitle_tracks,
                        request.selected_tracks,
                    ),
                    track_preference_key: request.track_preference_key,
                    remember_subtitle_on_start: request.remember_subtitle_on_start,
                },
                request.queue,
                request.emby.server.workspace_identity(),
                volume,
                error_message,
            ),
            backend_poll: Default::default(),
            emby: request.emby,
            report_effects,
            queue_effects,
        };
        if page.session.controls_view().error.is_some() {
            let _ = page.close_playback_reporting(true, false);
        }
        page
    }

    pub(super) fn register_image_cleanup(cx: &Context<Self>) {
        cx.on_release(|page, cx| {
            let images = page.presentation.release_images();
            defer_drop_released_images(images, cx);
        })
        .detach();
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    pub(super) fn back_to_detail(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.presentation.presentation_timers.close();
        self.session.cancel_poll();
        self.backend_poll.cancel();
        self.cancel_queue_switch();
        self.report_playback_progress(true);
        let update = self.close_playback_reporting(false, self.session.timeline().ended);
        defer_drop_subtitle(&mut self.presentation.subtitle, window);
        self.clear_visible_frame(window, cx);
        cx.emit(PlaybackEvent::Back { update });
    }

    pub(super) fn press_back_button(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.back_to_detail(window, cx);
    }

    pub(super) fn toggle_playback_fullscreen(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reset_fullscreen_controls();
        window.toggle_fullscreen();
        cx.notify();
    }
}

/// Page construction and in-memory fixtures share the same port binding path.
pub(super) fn bind_ports(
    context: &EmbyPlaybackContext,
    ports: crate::player::PlaybackPorts,
    cx: &gpui::App,
) -> (reporting::ReportingEffects, queue::QueueEffects) {
    (
        reporting::ReportingEffects::new(ports.reporting, context.server.workspace_identity(), cx),
        queue::QueueEffects::new(ports.source),
    )
}
