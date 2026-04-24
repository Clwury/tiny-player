use crate::media::PlaylistEntry;

pub struct AppState {
    pub playlist: Vec<PlaylistEntry>,
    pub selected_index: Option<usize>,
    pub is_playing: bool,
    pub error_message: Option<String>,
    pub playback_position_seconds: f64,
    pub playback_duration_seconds: f64,
}

impl AppState {
    pub fn from_playlist(playlist: Vec<PlaylistEntry>) -> Self {
        let selected_index = (!playlist.is_empty()).then_some(0);

        Self {
            playlist,
            selected_index,
            is_playing: false,
            error_message: None,
            playback_position_seconds: 0.0,
            playback_duration_seconds: 0.0,
        }
    }

    pub fn current_entry(&self) -> Option<&PlaylistEntry> {
        self.selected_index
            .and_then(|index| self.playlist.get(index))
    }

    pub fn current_title(&self) -> &str {
        self.current_entry()
            .map(|entry| entry.display_name.as_str())
            .unwrap_or("No video selected")
    }

    pub fn select(&mut self, index: usize) {
        if self.selected_index == Some(index) {
            return;
        }

        if self.playlist.get(index).is_some() {
            self.selected_index = Some(index);
            self.is_playing = false;
            self.playback_position_seconds = 0.0;
            self.playback_duration_seconds = 0.0;
            self.clear_error();
        }
    }

    pub fn sync_pause_state(&mut self, paused: bool) {
        self.is_playing = self.can_control_playback() && !paused;
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.is_playing = false;
        self.error_message = Some(message.into());
    }

    pub fn clear_error(&mut self) {
        self.error_message = None;
    }

    pub fn can_control_playback(&self) -> bool {
        self.current_entry().is_some()
    }

    pub fn has_previous(&self) -> bool {
        matches!(self.selected_index, Some(index) if index > 0)
    }

    pub fn has_next(&self) -> bool {
        matches!(
            self.selected_index,
            Some(index) if index
                .checked_add(1)
                .is_some_and(|next_index| next_index < self.playlist.len())
        )
    }

    pub fn update_progress(&mut self, position_seconds: f64, duration_seconds: f64) {
        self.playback_position_seconds = position_seconds.max(0.0);
        self.playback_duration_seconds = duration_seconds.max(0.0);
    }

    pub fn update_position(&mut self, position_seconds: f64) {
        self.playback_position_seconds = position_seconds.max(0.0);
    }

    pub fn update_duration(&mut self, duration_seconds: f64) {
        self.playback_duration_seconds = duration_seconds.max(0.0);
    }

    pub fn progress_fraction(&self) -> f32 {
        if self.playback_duration_seconds <= 0.0 {
            0.0
        } else {
            (self.playback_position_seconds / self.playback_duration_seconds).clamp(0.0, 1.0) as f32
        }
    }
}
