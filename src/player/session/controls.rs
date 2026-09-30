//! User intents and synchronous backend results. Native execution is injected;
//! this controller never holds a GPUI context, presenter, client or window.
use super::*;
use crate::player::model::{
    progress::{buffered_until_after_seek, cached_seek_target},
    time::clamp_playback_position,
    timeline::effective_playback_paused,
    volume::PlaybackVolumeState,
};
use tiny_playback::{
    BackendCommand, PlaybackRateChange, PlaybackSeekMode, PlaybackTrack, PlaybackTrackKind,
    PlaybackVolumeSettings, clamp_playback_volume,
};

pub(in crate::player) enum PlaybackIntent {
    TogglePause,
    AdjustVolume(f32),
    ToggleMute,
    ChangeRate(PlaybackRateChange),
    Seek {
        position: f64,
        mode: PlaybackSeekMode,
    },
    SeekRelative(f64),
    PreviewSeek(f64),
    SelectAudio(Option<usize>),
    SelectSubtitle(Option<PlaybackTrack>),
}

// Produced and consumed synchronously within dispatch_control. No callback can
// outlive the mutable session borrow; asynchronous backend events use reduce_backend.
pub(in crate::player) struct ControlCommand {
    pub(in crate::player) command: BackendCommand,
    pub(in crate::player) discard_frames: bool,
}

#[derive(Default)]
pub(in crate::player) struct ControlUpdate {
    pub(in crate::player) notify: bool,
    pub(in crate::player) report_progress: bool,
    pub(in crate::player) volume_changed: Option<PlaybackVolumeSettings>,
    pub(in crate::player) show_volume: bool,
    pub(in crate::player) show_rate: bool,
    pub(in crate::player) close_track_menu: bool,
    pub(in crate::player) clear_subtitle: bool,
    pub(in crate::player) remember_track: Option<PlaybackTrackKind>,
}

// Session-owned volume/rate/error. Construction restores persisted volume;
// intents commit synchronous command results; restart/end clear errors and
// backend/render failures replace them. Released with the session, never the UI.
pub(super) struct ControlState {
    pub(super) volume: PlaybackVolumeState,
    pub(super) rate: f64,
    pub(super) error: Option<String>,
}
impl ControlState {
    pub(super) fn new(volume: PlaybackVolumeSettings, error: Option<String>) -> Self {
        Self {
            volume: PlaybackVolumeState::new(volume),
            rate: 1.0,
            error,
        }
    }
}

pub(in crate::player) struct TrackChoices<'a> {
    pub(in crate::player) tracks: &'a [PlaybackTrack],
    pub(in crate::player) selected: Option<usize>,
}
impl TrackChoices<'_> {
    pub(in crate::player) fn enabled(&self) -> bool {
        !self.tracks.is_empty() || self.selected.is_some()
    }
}

pub(in crate::player) struct PlaybackControlsViewModel<'a> {
    pub(in crate::player) timeline: &'a PlaybackTimelineState,
    pub(in crate::player) can_toggle: bool,
    pub(in crate::player) can_seek: bool,
    pub(in crate::player) can_previous: bool,
    pub(in crate::player) can_next: bool,
    pub(in crate::player) audio: TrackChoices<'a>,
    pub(in crate::player) subtitles: TrackChoices<'a>,
    pub(in crate::player) volume: PlaybackVolumeSettings,
    pub(in crate::player) rate: f64,
    pub(in crate::player) error: Option<&'a str>,
}

impl PlaybackControlsViewModel<'_> {
    pub(in crate::player) fn progress(
        &self,
    ) -> Option<crate::player::model::progress::ProgressTimelineViewModel> {
        crate::player::model::progress::view_model(self.timeline)
    }
}

pub(in crate::player) struct TrackPreferenceUpdate<'a> {
    pub(in crate::player) keys: Vec<crate::player::PlaybackTrackPreferenceKey>,
    pub(in crate::player) track: Option<&'a PlaybackTrack>,
}

impl PlaybackSessionController {
    pub(in crate::player) fn pause_for_queue(
        &mut self,
        command: &crate::player::queue::QueueSwitchCommand,
        identity: &WorkspaceIdentity,
        execute: impl FnOnce(BackendCommand) -> Option<tiny_playback::Result<()>>,
    ) -> bool {
        if !command.pause_current {
            return true;
        }
        if let Some(Err(error)) = execute(BackendCommand::Pause) {
            self.queue
                .pause_failed(&command.token, identity, &error.to_string());
            return false;
        }
        self.timeline.user_paused = true;
        self.timeline.paused = true;
        self.timeline.buffering = false;
        true
    }

    pub(in crate::player) fn resume_after_queue_failure(
        &mut self,
        resume_current: bool,
        execute: impl FnOnce(BackendCommand) -> Option<tiny_playback::Result<()>>,
    ) -> bool {
        if !resume_current || self.timeline.ended {
            return false;
        }
        if let Some(Err(error)) = execute(BackendCommand::Resume) {
            self.queue.resume_failed(&error.to_string());
            return false;
        }
        self.timeline.user_paused = false;
        self.timeline.paused = effective_playback_paused(false, self.timeline.paused_for_cache);
        true
    }

    pub(in crate::player) fn track_preference_update(
        &self,
        kind: PlaybackTrackKind,
        item_id: &str,
        media_source_id: &str,
    ) -> Option<TrackPreferenceUpdate<'_>> {
        use crate::player::PlaybackTrackPreferenceKey;
        let view = self.controls_view();
        let choice = match kind {
            PlaybackTrackKind::Audio => view.audio,
            PlaybackTrackKind::Subtitle => view.subtitles,
        };
        let track = choice.selected.and_then(|index| {
            choice
                .tracks
                .iter()
                .find(|track| track.stream_index == index)
        });
        if choice.selected.is_some() && track.is_none() {
            return None;
        }
        let mut keys = vec![self.source.track_preference_key.clone()];
        // Grouped lists and resolved physical sources address the same choice.
        for item_id in std::iter::once(item_id).chain(
            self.queue
                .queue()
                .current()
                .map(|item| item.item_id.as_str()),
        ) {
            for source_id in [
                self.source.track_preference_key.media_source_id.as_str(),
                media_source_id,
            ] {
                let key = PlaybackTrackPreferenceKey {
                    item_id: item_id.to_owned(),
                    media_source_id: source_id.to_owned(),
                };
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
        Some(TrackPreferenceUpdate { keys, track })
    }

    pub(in crate::player) fn controls_view(&self) -> PlaybackControlsViewModel<'_> {
        let can_toggle =
            self.timeline.loaded && !self.timeline.ended && self.controls.error.is_none();
        PlaybackControlsViewModel {
            timeline: &self.timeline,
            can_toggle,
            can_seek: can_toggle && self.timeline.duration.is_some(),
            can_previous: self.queue.view_model().can_previous(),
            can_next: self.queue.view_model().can_next(),
            audio: TrackChoices {
                tracks: &self.source.tracks.audio,
                selected: self.source.tracks.selected_audio_stream_index,
            },
            subtitles: TrackChoices {
                tracks: &self.source.tracks.subtitles,
                selected: self.source.tracks.selected_subtitle_stream_index,
            },
            volume: self.controls.volume.settings(),
            rate: self.controls.rate,
            error: self.controls.error.as_deref(),
        }
    }

    pub(in crate::player) fn dispatch_control(
        &mut self,
        intent: PlaybackIntent,
        execute: impl FnOnce(ControlCommand) -> Option<tiny_playback::Result<()>>,
    ) -> ControlUpdate {
        match intent {
            PlaybackIntent::TogglePause => {
                if !self.controls_view().can_toggle {
                    return ControlUpdate::default();
                }
                let next = !self.timeline.user_paused;
                let Some(result) = execute(ControlCommand {
                    command: if next {
                        BackendCommand::Pause
                    } else {
                        BackendCommand::Resume
                    },
                    discard_frames: false,
                }) else {
                    return ControlUpdate::default();
                };
                let success = result.is_ok();
                if let Err(error) = result {
                    self.controls.error = Some(format!("控制播放失败：{error}"));
                } else {
                    self.timeline.user_paused = next;
                }
                self.timeline.paused = effective_playback_paused(
                    self.timeline.user_paused,
                    self.timeline.paused_for_cache,
                );
                if self.timeline.paused {
                    self.timeline.buffering = false;
                }
                ControlUpdate {
                    notify: true,
                    report_progress: success,
                    ..Default::default()
                }
            }
            PlaybackIntent::AdjustVolume(delta) => {
                self.set_volume(self.controls.volume.level + delta, execute)
            }
            PlaybackIntent::ToggleMute => {
                self.set_volume(self.controls.volume.level_after_mute_toggle(), execute)
            }
            PlaybackIntent::ChangeRate(change) => {
                let next = change.apply(self.controls.rate);
                let Some(result) = execute(ControlCommand {
                    command: BackendCommand::SetPlaybackRate { rate: next },
                    discard_frames: false,
                }) else {
                    return ControlUpdate::default();
                };
                let success = result.is_ok();
                if let Err(error) = result {
                    self.controls.error = Some(format!("调整播放速度失败：{error}"));
                } else {
                    self.controls.rate = next;
                }
                ControlUpdate {
                    notify: true,
                    show_rate: success,
                    ..Default::default()
                }
            }
            PlaybackIntent::SeekRelative(delta) => {
                if !self.controls_view().can_seek {
                    return ControlUpdate::default();
                }
                let position = self
                    .timeline
                    .progress_drag_position
                    .or(self.timeline.pending_seek_position)
                    .or(self.timeline.position)
                    .unwrap_or(0.0)
                    + delta;
                self.seek(position, PlaybackSeekMode::Fast, execute)
            }
            PlaybackIntent::Seek { position, mode } => self.seek(position, mode, execute),
            PlaybackIntent::PreviewSeek(position) => {
                let changed = self
                    .timeline
                    .progress_drag_position
                    .is_none_or(|old| (old - position).abs() >= 0.02);
                if changed {
                    self.timeline.progress_drag_position = Some(position);
                }
                ControlUpdate {
                    notify: changed,
                    ..Default::default()
                }
            }
            PlaybackIntent::SelectAudio(index) => {
                self.select_track(PlaybackTrackKind::Audio, index, None, execute)
            }
            PlaybackIntent::SelectSubtitle(track) => self.select_track(
                PlaybackTrackKind::Subtitle,
                track.as_ref().map(|track| track.stream_index),
                track,
                execute,
            ),
        }
    }

    fn set_volume(
        &mut self,
        level: f32,
        execute: impl FnOnce(ControlCommand) -> Option<tiny_playback::Result<()>>,
    ) -> ControlUpdate {
        let level = clamp_playback_volume(level);
        let mut update = ControlUpdate {
            show_volume: true,
            notify: true,
            ..Default::default()
        };
        if (self.controls.volume.level - level).abs() < f32::EPSILON {
            return update;
        }
        if let Some(Err(error)) = execute(ControlCommand {
            command: BackendCommand::SetVolume { volume: level },
            discard_frames: false,
        }) {
            self.controls.error = Some(format!("调整音量失败：{error}"));
            return update;
        }
        update.report_progress =
            (self.controls.volume.level <= f32::EPSILON) != (level <= f32::EPSILON);
        self.controls.volume.set_level(level);
        update.volume_changed = Some(self.controls.volume.settings());
        update
    }

    fn seek(
        &mut self,
        position: f64,
        mode: PlaybackSeekMode,
        execute: impl FnOnce(ControlCommand) -> Option<tiny_playback::Result<()>>,
    ) -> ControlUpdate {
        let position = self
            .timeline
            .duration
            .map(|duration| clamp_playback_position(position, duration))
            .unwrap_or(position);
        let cached = cached_seek_target(
            self.timeline.cache_state.as_ref(),
            self.timeline.buffered_until,
            self.timeline.position,
            position,
        );
        // Native dispatch is synchronous. Commit or roll back before yielding to
        // the page, preserving its previous observable post-command state.
        let result = execute(ControlCommand {
            command: BackendCommand::Seek {
                position_seconds: position,
                mode,
            },
            discard_frames: true,
        });
        let success = matches!(&result, Some(Ok(())));
        self.timeline.progress_drag_position = None;
        if success {
            self.timeline.ended = false;
            self.timeline.position = Some(position);
            self.timeline.buffered_until = if cached {
                buffered_until_after_seek(self.timeline.buffered_until, position)
            } else {
                super::super::model::time::valid_playback_time(position)
            };
            self.timeline.pending_seek_position = Some(position);
            self.timeline.pending_seek_keeps_frame = cached;
            self.timeline.buffering = self.timeline.loaded && !cached;
        } else {
            self.timeline.pending_seek_position = None;
            self.timeline.pending_seek_keeps_frame = false;
            self.timeline.buffering = false;
            if let Some(Err(error)) = result {
                self.controls.error = Some(format!("跳转播放位置失败：{error}"));
            }
        }
        ControlUpdate {
            notify: true,
            report_progress: success,
            ..Default::default()
        }
    }

    fn select_track(
        &mut self,
        kind: PlaybackTrackKind,
        index: Option<usize>,
        track: Option<PlaybackTrack>,
        execute: impl FnOnce(ControlCommand) -> Option<tiny_playback::Result<()>>,
    ) -> ControlUpdate {
        let position_seconds = self
            .timeline
            .progress_drag_position
            .or(self.timeline.position)
            .unwrap_or(0.0);
        let command = match kind {
            PlaybackTrackKind::Audio => BackendCommand::SetAudioTrack {
                track_index: index,
                position_seconds,
            },
            PlaybackTrackKind::Subtitle => BackendCommand::SetSubtitleTrack {
                track,
                position_seconds,
            },
        };
        let result = execute(ControlCommand {
            command,
            discard_frames: false,
        });
        let success = matches!(&result, Some(Ok(())));
        if success {
            match kind {
                PlaybackTrackKind::Audio => self.source.tracks.selected_audio_stream_index = index,
                PlaybackTrackKind::Subtitle => {
                    self.source.tracks.selected_subtitle_stream_index = index
                }
            }
            if index.is_some() {
                self.timeline.buffering = self.timeline.loaded;
            }
        } else {
            self.timeline.buffering = false;
            if let Some(Err(error)) = result {
                self.controls.error = Some(format!("切换轨道失败：{error}"));
            }
        }
        ControlUpdate {
            notify: true,
            report_progress: success,
            close_track_menu: true,
            clear_subtitle: success && kind == PlaybackTrackKind::Subtitle,
            remember_track: success.then_some(kind),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests;
