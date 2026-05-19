use super::*;

#[derive(Default)]
pub(super) struct PlaybackFrameState {
    pub(super) viewport_bounds: Option<Bounds<Pixels>>,
    pub(super) source_size: Option<RenderSize>,
    pub(super) current: Option<Arc<RenderImage>>,
}

pub(super) struct PlaybackTimelineState {
    pub(super) loaded: bool,
    pub(super) paused: bool,
    pub(super) buffering: bool,
    pub(super) position: Option<f64>,
    pub(super) duration: Option<f64>,
    pub(super) buffered_until: Option<f64>,
    pub(super) http_stream_buffered_range: Option<HttpStreamBufferProgress>,
    pub(super) pending_seek_position: Option<f64>,
    pub(super) pending_seek_keeps_frame: bool,
    pub(super) progress_track_bounds: Option<Bounds<Pixels>>,
    pub(super) progress_drag_position: Option<f64>,
}

impl Default for PlaybackTimelineState {
    fn default() -> Self {
        Self {
            loaded: false,
            paused: true,
            buffering: false,
            position: None,
            duration: None,
            buffered_until: None,
            http_stream_buffered_range: None,
            pending_seek_position: None,
            pending_seek_keeps_frame: false,
            progress_track_bounds: None,
            progress_drag_position: None,
        }
    }
}

#[derive(Default)]
pub(super) struct FullscreenControlsState {
    pub(super) cursor_visible: bool,
    pub(super) controls_visible: bool,
    pub(super) mouse_in_controls: bool,
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
