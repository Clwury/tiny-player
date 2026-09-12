pub(crate) const MIN_PLAYBACK_RATE: f64 = 0.25;
pub(crate) const MAX_PLAYBACK_RATE: f64 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaybackRateChange {
    Decrease,
    Increase,
    Halve,
    Double,
    Reset,
}

impl PlaybackRateChange {
    pub(crate) fn apply(self, rate: f64) -> f64 {
        let rate = clamp_playback_rate(rate);
        clamp_playback_rate(match self {
            Self::Decrease => rate / 1.1,
            Self::Increase => rate * 1.1,
            Self::Halve => rate * 0.5,
            Self::Double => rate * 2.0,
            Self::Reset => 1.0,
        })
    }
}

pub(crate) fn clamp_playback_rate(rate: f64) -> f64 {
    if rate.is_finite() {
        rate.clamp(MIN_PLAYBACK_RATE, MAX_PLAYBACK_RATE)
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_rate_bounds_reject_invalid_and_extreme_values() {
        for rate in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(clamp_playback_rate(rate), 1.0);
        }
        assert_eq!(clamp_playback_rate(-1.0), 0.25);
        assert_eq!(clamp_playback_rate(0.0), 0.25);
        assert_eq!(clamp_playback_rate(8.0), 4.0);
        assert_eq!(clamp_playback_rate(1.1), 1.1);
    }

    #[test]
    fn mpv_rate_changes_multiply_the_current_rate_and_reset_to_normal() {
        use PlaybackRateChange::*;
        assert!((Increase.apply(1.5) - 1.65).abs() < 1e-9);
        assert!((Decrease.apply(Increase.apply(1.5)) - 1.5).abs() < 1e-9);
        assert_eq!(Halve.apply(1.5), 0.75);
        assert_eq!(Double.apply(1.5), 3.0);
        assert_eq!(Double.apply(3.0), MAX_PLAYBACK_RATE);
        assert_eq!(Halve.apply(0.25), MIN_PLAYBACK_RATE);
        assert_eq!(Reset.apply(0.25), 1.0);
        assert_eq!(Reset.apply(4.0), 1.0);
    }
}
