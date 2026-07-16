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
    pub(super) indicator_visible: bool,
    pub(super) hide_generation: u64,
}

impl Default for PlaybackVolumeState {
    fn default() -> Self {
        Self {
            level: 1.0,
            indicator_visible: false,
            hide_generation: 0,
        }
    }
}
