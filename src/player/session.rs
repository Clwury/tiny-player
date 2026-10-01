//! Session business owner and backend event reducer. It produces ordered report
//! commands before resetting the timeline; GPUI owns presentation and execution.

mod controls;
mod episode_cards;
use super::{
    model::{
        source::PlaybackSourceState,
        timeline::{
            PlaybackTimelineState, apply_cache_buffering_to_timeline,
            apply_paused_for_cache_to_timeline, apply_playback_restart_to_timeline,
        },
    },
    queue::QueueController,
    reporting::{
        ReportContext, ReportTelemetry, ReportingController, ReportingIntent, ReportingTransition,
    },
};
use crate::{
    effects::{RequestScope, RequestSlot, RequestToken, WorkspaceIdentity},
    media::PlaybackQueue,
};
use controls::ControlState;
pub(super) use controls::{ControlUpdate, PlaybackIntent};
use tiny_playback::{BackendDiagnostic, BackendEventKind, BackendSubtitleCue, RenderSize};

/// Session-owned business domains. Backend results and user intents update the
/// same owner; the page borrows selectors and executes effects. Released with page.
pub(super) struct PlaybackSessionController {
    timeline: PlaybackTimelineState,
    source: PlaybackSourceState,
    pub(super) queue: QueueController,
    pub(super) reporting: ReportingController,
    controls: ControlState,
    // One cancellable paused-cache poll per session; end/failure/release invalidate it.
    poll: RequestSlot,
    poll_scheduled: bool,
}

pub(super) struct BackendContext<'a> {
    pub(super) item_id: &'a str,
    pub(super) media_source_id: &'a str,
    pub(super) play_session_id: Option<&'a str>,
    pub(super) run_time_ticks: Option<u64>,
    pub(super) has_frame: bool,
}

#[derive(Default)]
pub(super) enum BackendAction {
    #[default]
    None,
    Restart,
    Failed,
    Ended {
        auto_next: bool,
    },
    Diagnostic(BackendDiagnostic),
    TracksChanged,
    Subtitle(Option<BackendSubtitleCue>),
    VideoSize(Option<RenderSize>),
}

#[derive(Default)]
pub(super) struct BackendTransition {
    pub(super) reporting: Vec<ReportingTransition>,
    pub(super) action: BackendAction,
}

impl PlaybackSessionController {
    pub(super) fn timeline(&self) -> &PlaybackTimelineState {
        &self.timeline
    }
    pub(super) fn source_view(&self) -> &PlaybackSourceState {
        &self.source
    }
    pub(super) fn take_start_subtitle_preference(&mut self) -> bool {
        std::mem::take(&mut self.source.remember_subtitle_on_start)
    }

    #[cfg(test)]
    pub(super) fn timeline_mut(&mut self) -> &mut PlaybackTimelineState {
        &mut self.timeline
    }
    #[cfg(test)]
    pub(super) fn source_mut(&mut self) -> &mut PlaybackSourceState {
        &mut self.source
    }

    pub(super) fn new(
        timeline: PlaybackTimelineState,
        source: PlaybackSourceState,
        queue: PlaybackQueue,
        identity: WorkspaceIdentity,
        volume: tiny_playback::PlaybackVolumeSettings,
        error: Option<String>,
    ) -> Self {
        Self {
            timeline,
            source,
            queue: QueueController::new(queue, identity.clone()),
            reporting: ReportingController::new(identity.clone()),
            poll: RequestSlot::new(RequestScope::PlaybackBackendPoll, identity),
            poll_scheduled: false,
            controls: ControlState::new(volume, error),
        }
    }

    pub(super) fn reduce_backend(
        &mut self,
        event: BackendEventKind,
        context: BackendContext<'_>,
    ) -> BackendTransition {
        let mut transition = BackendTransition::default();
        transition.action = match event {
            BackendEventKind::PlaybackRestart => {
                if self.reporting.is_closed() {
                    return transition;
                }
                apply_playback_restart_to_timeline(&mut self.timeline);
                self.controls.error = None;
                transition
                    .reporting
                    .push(self.report(ReportingIntent::Restart, &context));
                BackendAction::Restart
            }
            BackendEventKind::LoadFailed(message) => {
                self.fail(&context, &mut transition);
                self.controls.error = Some(format!("加载视频失败：{message}"));
                BackendAction::Failed
            }
            BackendEventKind::Fatal(message) => {
                self.fail(&context, &mut transition);
                self.controls.error = Some(format!("播放后端错误：{message}"));
                BackendAction::Failed
            }
            BackendEventKind::PlaybackEnded => {
                if self.timeline.ended || self.reporting.is_closed() {
                    return transition;
                }
                let fallback_duration = context
                    .run_time_ticks
                    .filter(|ticks| *ticks > 0)
                    .map(|ticks| ticks as f64 / 10_000_000_f64)
                    .filter(|seconds| seconds.is_finite() && *seconds > 0.0);
                self.timeline.prepare_end_report(fallback_duration);
                transition
                    .reporting
                    .push(self.report(ReportingIntent::Progress { force: true }, &context));
                transition.reporting.push(self.report(
                    ReportingIntent::Close {
                        failed: false,
                        ended: true,
                    },
                    &context,
                ));
                self.timeline.finish_playback();
                self.cancel_poll();
                self.controls.error = None;
                BackendAction::Ended {
                    auto_next: self.queue.view_model().can_next(),
                }
            }
            BackendEventKind::Pause(paused) => {
                self.timeline.apply_pause(paused);
                transition
                    .reporting
                    .push(self.report(ReportingIntent::Progress { force: false }, &context));
                BackendAction::None
            }
            BackendEventKind::Buffering(buffering) => {
                self.timeline.apply_buffering(buffering, context.has_frame);
                BackendAction::None
            }
            BackendEventKind::PositionChanged(position) => {
                self.timeline.apply_position(position);
                BackendAction::None
            }
            BackendEventKind::DurationChanged(duration) => {
                self.timeline.apply_duration(duration);
                BackendAction::None
            }
            BackendEventKind::BufferedChanged(buffered) => {
                self.timeline.apply_buffered(buffered);
                BackendAction::None
            }
            BackendEventKind::CacheStateChanged(state) => {
                self.timeline.apply_cache_state(state);
                BackendAction::None
            }
            BackendEventKind::PausedForCacheChanged(paused) => {
                apply_paused_for_cache_to_timeline(&mut self.timeline, paused);
                BackendAction::None
            }
            BackendEventKind::CacheBufferingChanged(percent) => {
                apply_cache_buffering_to_timeline(&mut self.timeline, percent);
                BackendAction::None
            }
            BackendEventKind::Diagnostic(diagnostic) => BackendAction::Diagnostic(diagnostic),
            BackendEventKind::PlaybackInfoChanged(info) => {
                self.source.playback_info = info;
                BackendAction::None
            }
            BackendEventKind::PlaybackFileInfoChanged(info) => {
                self.source.playback_file_info = Some(info);
                BackendAction::None
            }
            BackendEventKind::PlaybackAudioInfoChanged(info) => {
                self.source.playback_audio_info = info;
                BackendAction::None
            }
            BackendEventKind::PlaybackTracksChanged {
                audio,
                subtitles,
                selected,
            } => {
                self.source.apply_tracks(audio, subtitles, selected);
                BackendAction::TracksChanged
            }
            BackendEventKind::SubtitleChanged(cue) => BackendAction::Subtitle(cue),
            BackendEventKind::VideoSizeChanged(size) => {
                if let (Some(info), Some(size)) = (self.source.playback_info.as_mut(), size) {
                    info.size = size;
                }
                BackendAction::VideoSize(size)
            }
        };
        transition
    }

    pub(super) fn should_poll(&self, has_backend: bool, has_error: bool) -> bool {
        has_backend
            && !self.reporting.is_closed()
            && self.timeline.loaded
            && self.timeline.paused
            && !self.timeline.ended
            && !has_error
            && self
                .timeline
                .cache_state
                .as_ref()
                .is_some_and(cache_state_needs_poll)
    }
    pub(super) fn begin_poll(
        &mut self,
        has_backend: bool,
        has_error: bool,
    ) -> Option<RequestToken> {
        if self.poll_scheduled || !self.should_poll(has_backend, has_error) {
            return None;
        }
        self.poll_scheduled = true;
        Some(self.poll.issue())
    }
    pub(super) fn cancel_poll(&mut self) {
        self.poll.invalidate();
        self.poll_scheduled = false;
    }
    pub(super) fn complete_poll(
        &mut self,
        token: &RequestToken,
        identity: &WorkspaceIdentity,
        has_backend: bool,
        has_error: bool,
    ) -> bool {
        if !token.is_for(identity) || !self.poll.commit(token) {
            return false;
        }
        self.poll_scheduled = false;
        self.should_poll(has_backend, has_error)
    }

    pub(super) fn report(
        &mut self,
        intent: ReportingIntent,
        context: &BackendContext<'_>,
    ) -> ReportingTransition {
        self.reporting.dispatch(
            intent,
            ReportContext {
                item_id: context.item_id,
                media_source_id: context.media_source_id,
                play_session_id: context.play_session_id,
                run_time_ticks: context.run_time_ticks,
                queue: self.queue.queue(),
            },
            ReportTelemetry {
                can_seek: self.timeline.loaded
                    && !self.timeline.ended
                    && self.controls.error.is_none()
                    && self.timeline.duration.is_some(),
                audio: self.source.tracks.selected_audio_stream_index,
                subtitle: self.source.tracks.selected_subtitle_stream_index,
                user_paused: self.timeline.user_paused,
                volume: self.controls.volume.level,
                position: self.timeline.position,
                drag_position: self.timeline.progress_drag_position,
                duration: self.timeline.duration,
            },
        )
    }
    fn fail(&mut self, context: &BackendContext<'_>, transition: &mut BackendTransition) {
        self.queue.cancel();
        transition.reporting.push(self.report(
            ReportingIntent::Close {
                failed: true,
                ended: false,
            },
            context,
        ));
        self.timeline.fail_playback();
        self.cancel_poll();
        self.source.clear_metadata();
    }

    pub(super) fn render_failed(
        &mut self,
        message: String,
        context: BackendContext<'_>,
    ) -> ReportingTransition {
        self.timeline.user_paused = true;
        self.timeline.paused = true;
        self.queue.cancel();
        let transition = self.report(
            ReportingIntent::Close {
                failed: true,
                ended: false,
            },
            &context,
        );
        self.controls.error = Some(format!("渲染视频失败：{message}"));
        transition
    }

    #[cfg(test)]
    pub(super) fn clear_error(&mut self) {
        self.controls.error = None;
    }
}

pub(super) fn cache_state_needs_poll(state: &tiny_playback::PlaybackCacheState) -> bool {
    if state.paused_for_cache || state.buffering_percent.is_some() {
        return true;
    }
    if state.byte.as_ref().is_some_and(|byte| !byte.idle) {
        return true;
    }
    !state.demux.idle && !state.demux.eof
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
