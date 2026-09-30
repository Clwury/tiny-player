pub(crate) fn valid_playback_time(time: f64) -> Option<f64> {
    (time.is_finite() && time >= 0.0).then_some(time)
}

pub(crate) fn valid_playback_duration(duration: f64) -> Option<f64> {
    (duration.is_finite() && duration > 0.0).then_some(duration)
}

pub(crate) fn clamp_playback_position(position: f64, duration: f64) -> f64 {
    if !position.is_finite() {
        return 0.0;
    }
    position.clamp(0.0, duration.max(0.0))
}

pub(crate) fn should_apply_backend_position(
    progress_drag_position: Option<f64>,
    pending_seek_position: Option<f64>,
) -> bool {
    progress_drag_position.is_none() && pending_seek_position.is_none()
}

/// Emby playback positions and durations use 100 ns ticks.
pub(crate) const EMBY_TICKS_PER_SECOND: u64 = 10_000_000;

pub fn playback_initial_position_seconds(
    playback_position_ticks: Option<u64>,
    run_time_ticks: Option<u64>,
) -> f64 {
    let Some(position_ticks) = playback_position_ticks.filter(|ticks| *ticks > 0) else {
        return 0.0;
    };
    if run_time_ticks.is_some_and(|runtime| runtime == 0 || position_ticks >= runtime) {
        return 0.0;
    }

    let seconds = position_ticks as f64 / EMBY_TICKS_PER_SECOND as f64;
    if seconds.is_finite() && seconds > 0.0 {
        seconds
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_position_resumes_only_before_known_runtime_end() {
        assert_eq!(playback_initial_position_seconds(None, None), 0.0);
        assert_eq!(playback_initial_position_seconds(Some(0), None), 0.0);
        assert_eq!(
            playback_initial_position_seconds(Some(120_000_000), Some(600_000_000)),
            12.0
        );
        assert_eq!(
            playback_initial_position_seconds(Some(600_000_000), Some(600_000_000)),
            0.0
        );
        assert_eq!(
            playback_initial_position_seconds(Some(700_000_000), Some(600_000_000)),
            0.0
        );
        assert_eq!(
            playback_initial_position_seconds(Some(120_000_000), None),
            12.0
        );
    }
}
