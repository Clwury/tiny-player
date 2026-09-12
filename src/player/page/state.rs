use super::*;

#[derive(Default)]
pub(super) struct PlaybackFrameState {
    pub(super) viewport_bounds: Option<Bounds<Pixels>>,
    pub(super) source_size: Option<RenderSize>,
    pub(super) current: Option<Arc<RenderImage>>,
}

pub(super) struct PlaybackTimelineState {
    pub(super) loaded: bool,
    pub(super) ended: bool,
    pub(super) user_paused: bool,
    pub(super) paused: bool,
    pub(super) buffering: bool,
    pub(super) position: Option<f64>,
    pub(super) duration: Option<f64>,
    pub(super) buffered_until: Option<f64>,
    pub(super) cache_state: Option<PlaybackCacheState>,
    pub(super) cache_status_open: bool,
    pub(super) paused_for_cache: bool,
    pub(super) cache_buffering_percent: Option<u8>,
    pub(super) paused_backend_poll_scheduled: bool,
    pub(super) pending_seek_position: Option<f64>,
    pub(super) pending_seek_keeps_frame: bool,
    pub(super) progress_track_bounds: Option<Bounds<Pixels>>,
    pub(super) progress_drag_position: Option<f64>,
}

impl Default for PlaybackTimelineState {
    fn default() -> Self {
        Self {
            loaded: false,
            ended: false,
            user_paused: true,
            paused: true,
            buffering: false,
            position: None,
            duration: None,
            buffered_until: None,
            cache_state: None,
            cache_status_open: false,
            paused_for_cache: false,
            cache_buffering_percent: None,
            paused_backend_poll_scheduled: false,
            pending_seek_position: None,
            pending_seek_keeps_frame: false,
            progress_track_bounds: None,
            progress_drag_position: None,
        }
    }
}

pub(super) fn effective_playback_paused(user_paused: bool, paused_for_cache: bool) -> bool {
    user_paused || paused_for_cache
}

pub(super) fn user_pause_from_effective_pause_event(
    current_user_paused: bool,
    paused_for_cache: bool,
    effective_paused: bool,
) -> bool {
    if paused_for_cache {
        current_user_paused
    } else {
        effective_paused
    }
}

#[derive(Default)]
pub(super) struct FullscreenControlsState {
    pub(super) cursor_visible: bool,
    pub(super) controls_visible: bool,
    pub(super) mouse_in_controls: bool,
    pub(super) mouse_in_back_button: bool,
    pub(super) hide_generation: u64,
}

pub(super) struct TrackSelectState {
    pub(super) audio: Vec<PlaybackTrack>,
    pub(super) subtitles: Vec<PlaybackTrack>,
    pub(super) selected_audio_stream_index: Option<usize>,
    pub(super) selected_subtitle_stream_index: Option<usize>,
    pub(super) open: Option<PlaybackTrackKind>,
}

impl TrackSelectState {
    pub(super) fn new(
        audio: Vec<PlaybackTrack>,
        subtitles: Vec<PlaybackTrack>,
        selected: PlaybackTrackSelection,
    ) -> Self {
        Self {
            audio,
            subtitles,
            selected_audio_stream_index: selected.audio_stream_index,
            selected_subtitle_stream_index: selected.subtitle_stream_index,
            open: None,
        }
    }
}

#[derive(Default)]
pub(super) struct SubtitleOverlayState {
    pub(super) active: Option<BackendSubtitleCue>,
    pub(super) vertical_offset_fraction: Option<f32>,
}

pub(super) struct PlaybackVolumeState {
    pub(super) level: f32,
    unmuted_level: f32,
    pub(super) indicator_visible: bool,
    pub(super) hide_generation: u64,
}

impl Default for PlaybackVolumeState {
    fn default() -> Self {
        Self::new(PlaybackVolumeSettings::default())
    }
}

impl PlaybackVolumeState {
    pub(super) fn new(settings: PlaybackVolumeSettings) -> Self {
        let settings = settings.normalized();
        Self {
            level: settings.level,
            unmuted_level: settings.unmuted_level,
            indicator_visible: false,
            hide_generation: 0,
        }
    }

    pub(super) fn settings(&self) -> PlaybackVolumeSettings {
        PlaybackVolumeSettings {
            level: self.level,
            unmuted_level: self.unmuted_level,
        }
    }

    pub(super) fn set_level(&mut self, level: f32) {
        self.level = clamp_playback_volume(level);
        if self.level > f32::EPSILON {
            self.unmuted_level = self.level;
        }
    }

    pub(super) fn level_after_mute_toggle(&self) -> f32 {
        if self.level > f32::EPSILON {
            0.0
        } else {
            self.unmuted_level
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mute_toggle_restores_the_previous_volume() {
        let mut volume = PlaybackVolumeState::default();
        volume.set_level(0.42);
        volume.set_level(volume.level_after_mute_toggle());
        assert_eq!(volume.level, 0.0);
        volume.set_level(volume.level_after_mute_toggle());
        assert_eq!(volume.level, 0.42);
    }

    #[test]
    fn mute_toggle_uses_the_latest_adjusted_volume() {
        let mut volume = PlaybackVolumeState::default();
        volume.set_level(volume.level_after_mute_toggle());
        volume.set_level(volume.level + PLAYBACK_VOLUME_STEP);
        assert_eq!(volume.level, 0.02);
        volume.set_level(volume.level_after_mute_toggle());
        assert_eq!(volume.level, 0.0);
        volume.set_level(volume.level_after_mute_toggle());
        assert_eq!(volume.level, 0.02);
    }

    #[test]
    fn restored_volume_preserves_mute_and_the_previous_level() {
        let mut volume = PlaybackVolumeState::default();
        volume.set_level(0.42);
        let restored = PlaybackVolumeState::new(volume.settings());
        assert_eq!(restored.level, 0.42);
        assert!(!restored.indicator_visible);

        volume.set_level(volume.level_after_mute_toggle());
        let mut restored = PlaybackVolumeState::new(volume.settings());
        assert_eq!(restored.level, 0.0);
        restored.set_level(restored.level_after_mute_toggle());
        assert_eq!(restored.level, 0.42);
    }
}
