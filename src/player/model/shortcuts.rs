use tiny_playback::PlaybackRateChange;

pub(in crate::player) const SEEK_STEP_SECONDS: i32 = 5;
pub(in crate::player) const LONG_SEEK_STEP_SECONDS: i32 = 60;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::player) enum PlaybackShortcut {
    TogglePlayback,
    ToggleFullscreen,
    ExitFullscreen,
    Back,
    ToggleControls,
    SeekRelative(i32),
    ToggleInfoOverlay,
    RaiseSubtitle,
    LowerSubtitle,
    DecreaseVolume,
    IncreaseVolume,
    ToggleMute,
    ChangeRate(PlaybackRateChange),
}

impl PlaybackShortcut {
    pub(super) fn gamepad_repeat_interval(self) -> Option<std::time::Duration> {
        match self {
            Self::SeekRelative(_) => Some(std::time::Duration::from_millis(250)),
            Self::DecreaseVolume | Self::IncreaseVolume => {
                Some(std::time::Duration::from_millis(150))
            }
            _ => None,
        }
    }
}
