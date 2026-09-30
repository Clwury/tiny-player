use tiny_playback::PlaybackCacheState;

use super::time::{
    clamp_playback_position, should_apply_backend_position, valid_playback_duration,
    valid_playback_time,
};

/// Owned by PlaybackSessionController. Backend events reduce through that
/// owner; transport controls are migrating to intents. Released with the session.
pub(crate) struct PlaybackTimelineState {
    pub(crate) loaded: bool,
    pub(crate) ended: bool,
    pub(crate) user_paused: bool,
    pub(crate) paused: bool,
    pub(crate) buffering: bool,
    pub(crate) position: Option<f64>,
    pub(crate) duration: Option<f64>,
    pub(crate) buffered_until: Option<f64>,
    pub(crate) cache_state: Option<PlaybackCacheState>,
    pub(crate) paused_for_cache: bool,
    pub(crate) cache_buffering_percent: Option<u8>,
    pub(crate) pending_seek_position: Option<f64>,
    pub(crate) pending_seek_keeps_frame: bool,
    pub(crate) progress_drag_position: Option<f64>,
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
            paused_for_cache: false,
            cache_buffering_percent: None,
            pending_seek_position: None,
            pending_seek_keeps_frame: false,
            progress_drag_position: None,
        }
    }
}

impl PlaybackTimelineState {
    /// Reduce the end report before closing the reporter; clearing seek/drag
    /// prevents a pending preview from replacing the terminal position.
    pub(crate) fn prepare_end_report(&mut self, fallback_duration: Option<f64>) {
        self.progress_drag_position = None;
        self.pending_seek_position = None;
        self.pending_seek_keeps_frame = false;
        if let Some(position) = self.duration.or(fallback_duration) {
            self.position = Some(position);
            self.buffered_until = Some(position);
        }
    }

    pub(crate) fn finish_playback(&mut self) {
        self.loaded = true;
        self.ended = true;
        self.clear_activity();
        if let Some(duration) = self.duration {
            self.position = Some(duration);
            self.buffered_until = Some(duration);
        }
    }

    pub(crate) fn fail_playback(&mut self) {
        self.loaded = false;
        self.ended = false;
        self.clear_activity();
        self.buffered_until = None;
    }

    fn clear_activity(&mut self) {
        self.user_paused = true;
        self.paused = true;
        self.buffering = false;
        self.cache_state = None;
        self.paused_for_cache = false;
        self.cache_buffering_percent = None;
        self.pending_seek_position = None;
        self.pending_seek_keeps_frame = false;
        self.progress_drag_position = None;
    }

    pub(crate) fn apply_pause(&mut self, paused: bool) {
        self.user_paused =
            user_pause_from_effective_pause_event(self.user_paused, self.paused_for_cache, paused);
        self.paused = effective_playback_paused(self.user_paused, self.paused_for_cache);
    }

    pub(crate) fn apply_buffering(&mut self, buffering: bool, has_frame: bool) {
        let hidden_by_soft_seek = buffering && self.pending_seek_keeps_frame && has_frame;
        self.buffering = buffering && !hidden_by_soft_seek;
    }

    pub(crate) fn apply_position(&mut self, position: f64) {
        if should_apply_backend_position(self.progress_drag_position, self.pending_seek_position) {
            self.position = valid_playback_time(position);
        }
    }

    pub(crate) fn apply_duration(&mut self, duration: f64) {
        self.duration = valid_playback_duration(duration);
        if let (Some(drag_position), Some(duration)) = (self.progress_drag_position, self.duration)
        {
            self.progress_drag_position = Some(clamp_playback_position(drag_position, duration));
        }
    }

    pub(crate) fn apply_buffered(&mut self, buffered_until: Option<f64>) {
        let buffered_until = buffered_until.and_then(valid_playback_time);
        self.buffered_until = if self.pending_seek_keeps_frame {
            match (self.buffered_until, buffered_until) {
                (Some(current), Some(next)) => Some(current.max(next)),
                (_, next) => next,
            }
        } else {
            buffered_until
        };
    }

    pub(crate) fn apply_cache_state(&mut self, state: PlaybackCacheState) {
        self.buffered_until = state.demux.cache_end.and_then(valid_playback_time);
        self.paused_for_cache = state.paused_for_cache;
        self.paused = effective_playback_paused(self.user_paused, state.paused_for_cache);
        self.cache_buffering_percent = state.buffering_percent;
        self.cache_state = Some(state);
    }
}

pub(crate) fn effective_playback_paused(user_paused: bool, paused_for_cache: bool) -> bool {
    user_paused || paused_for_cache
}

pub(crate) fn user_pause_from_effective_pause_event(
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

pub(crate) fn apply_playback_restart_to_timeline(timeline: &mut PlaybackTimelineState) {
    let first_restart = !timeline.loaded;
    let paused_for_cache = timeline.paused_for_cache;
    let cache_buffering_percent = timeline.cache_buffering_percent;
    timeline.loaded = true;
    timeline.ended = false;
    if first_restart {
        timeline.user_paused = false;
    }
    timeline.paused = effective_playback_paused(timeline.user_paused, paused_for_cache);
    timeline.buffering = false;
    // A restart also marks the first frame after a seek. The cache state emitted
    // earlier in the same poll remains authoritative until the next cache tick.
    timeline.paused_for_cache = paused_for_cache;
    timeline.cache_buffering_percent = cache_buffering_percent.filter(|_| paused_for_cache);
    timeline.pending_seek_position = None;
    timeline.pending_seek_keeps_frame = false;
}

pub(crate) fn apply_paused_for_cache_to_timeline(
    timeline: &mut PlaybackTimelineState,
    paused_for_cache: bool,
) {
    timeline.paused_for_cache = paused_for_cache;
    timeline.paused = effective_playback_paused(timeline.user_paused, paused_for_cache);
    if !paused_for_cache {
        timeline.cache_buffering_percent = None;
    }
    if let Some(cache_state) = timeline.cache_state.as_mut() {
        cache_state.paused_for_cache = paused_for_cache;
        if !paused_for_cache {
            cache_state.buffering_percent = None;
        }
    }
}

pub(crate) fn apply_cache_buffering_to_timeline(
    timeline: &mut PlaybackTimelineState,
    percent: Option<u8>,
) {
    timeline.cache_buffering_percent = percent;
    if let Some(cache_state) = timeline.cache_state.as_mut() {
        cache_state.buffering_percent = percent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_report_uses_duration_then_fallback_and_discards_seek_preview() {
        for (duration, fallback, expected) in [
            (Some(120.0), Some(90.0), Some(120.0)),
            (None, Some(90.0), Some(90.0)),
            (None, None, Some(12.0)),
        ] {
            let mut timeline = PlaybackTimelineState {
                duration,
                position: Some(12.0),
                progress_drag_position: Some(30.0),
                pending_seek_position: Some(30.0),
                pending_seek_keeps_frame: true,
                ..Default::default()
            };
            timeline.prepare_end_report(fallback);
            assert_eq!(timeline.position, expected);
            assert!(timeline.progress_drag_position.is_none());
            assert!(timeline.pending_seek_position.is_none());
            assert!(!timeline.pending_seek_keeps_frame);
            timeline.finish_playback();
            assert_eq!(timeline.position, expected);
            assert!(timeline.loaded && timeline.ended && timeline.paused);
        }
    }

    #[test]
    fn backend_failure_keeps_last_position_but_clears_pending_activity() {
        let mut timeline = PlaybackTimelineState {
            loaded: true,
            ended: true,
            position: Some(42.0),
            duration: Some(120.0),
            buffered_until: Some(80.0),
            buffering: true,
            cache_state: Some(Default::default()),
            paused_for_cache: true,
            cache_buffering_percent: Some(15),
            pending_seek_position: Some(100.0),
            progress_drag_position: Some(101.0),
            ..Default::default()
        };
        timeline.fail_playback();
        assert_eq!(timeline.position, Some(42.0));
        assert_eq!(timeline.duration, Some(120.0));
        assert!(!timeline.loaded && !timeline.ended && !timeline.buffering);
        assert!(timeline.user_paused && timeline.paused);
        assert!(timeline.cache_state.is_none());
        assert!(timeline.buffered_until.is_none());
        assert!(timeline.pending_seek_position.is_none());
        assert!(timeline.progress_drag_position.is_none());
        assert!(!timeline.paused_for_cache);
        assert!(timeline.cache_buffering_percent.is_none());
    }

    #[test]
    fn cache_pause_events_do_not_change_user_pause_intent() {
        let mut timeline = PlaybackTimelineState::default();
        apply_playback_restart_to_timeline(&mut timeline);
        apply_paused_for_cache_to_timeline(&mut timeline, true);
        timeline.apply_pause(true);
        assert!(!timeline.user_paused);
        assert!(timeline.paused);
        apply_paused_for_cache_to_timeline(&mut timeline, false);
        assert!(!timeline.paused);
        timeline.apply_pause(true);
        apply_paused_for_cache_to_timeline(&mut timeline, true);
        apply_paused_for_cache_to_timeline(&mut timeline, false);
        assert!(timeline.user_paused);
        assert!(timeline.paused);
    }

    #[test]
    fn seek_and_drag_own_position_until_restart_finishes_the_seek() {
        let mut timeline = PlaybackTimelineState {
            loaded: true,
            position: Some(10.0),
            pending_seek_position: Some(80.0),
            pending_seek_keeps_frame: true,
            ..PlaybackTimelineState::default()
        };
        timeline.apply_position(12.0);
        assert_eq!(timeline.position, Some(10.0));
        timeline.apply_buffering(true, true);
        assert!(!timeline.buffering);
        apply_playback_restart_to_timeline(&mut timeline);
        timeline.apply_position(80.0);
        assert_eq!(timeline.position, Some(80.0));
        timeline.progress_drag_position = Some(500.0);
        timeline.apply_duration(120.0);
        assert_eq!(timeline.progress_drag_position, Some(120.0));
        timeline.apply_position(82.0);
        assert_eq!(timeline.position, Some(80.0));
        timeline.progress_drag_position = None;
        timeline.apply_position(f64::NAN);
        assert_eq!(timeline.position, None);
    }

    #[test]
    fn soft_seek_preserves_buffer_extent_until_authoritative_cache_update() {
        let mut timeline = PlaybackTimelineState {
            buffered_until: Some(90.0),
            pending_seek_keeps_frame: true,
            ..PlaybackTimelineState::default()
        };
        timeline.apply_buffered(Some(50.0));
        assert_eq!(timeline.buffered_until, Some(90.0));
        timeline.apply_cache_state(PlaybackCacheState {
            demux: tiny_playback::DemuxCacheState {
                cache_end: Some(55.0),
                ..Default::default()
            },
            paused_for_cache: true,
            buffering_percent: Some(30),
            ..Default::default()
        });
        assert_eq!(timeline.buffered_until, Some(55.0));
        apply_playback_restart_to_timeline(&mut timeline);
        assert!(timeline.paused_for_cache);
        assert_eq!(timeline.cache_buffering_percent, Some(30));
        apply_paused_for_cache_to_timeline(&mut timeline, false);
        assert_eq!(timeline.cache_buffering_percent, None);
        assert_eq!(timeline.cache_state.unwrap().buffering_percent, None);
    }
}
