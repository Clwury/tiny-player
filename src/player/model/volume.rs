use tiny_playback::{PlaybackVolumeSettings, clamp_playback_volume};

pub(in crate::player) struct PlaybackVolumeState {
    pub(in crate::player) level: f32,
    unmuted_level: f32,
}

impl Default for PlaybackVolumeState {
    fn default() -> Self {
        Self::new(PlaybackVolumeSettings::default())
    }
}

impl PlaybackVolumeState {
    pub(in crate::player) fn new(settings: PlaybackVolumeSettings) -> Self {
        let settings = settings.normalized();
        Self {
            level: settings.level,
            unmuted_level: settings.unmuted_level,
        }
    }

    pub(in crate::player) fn settings(&self) -> PlaybackVolumeSettings {
        PlaybackVolumeSettings {
            level: self.level,
            unmuted_level: self.unmuted_level,
        }
    }

    pub(in crate::player) fn set_level(&mut self, level: f32) {
        self.level = clamp_playback_volume(level);
        if self.level > f32::EPSILON {
            self.unmuted_level = self.level;
        }
    }

    pub(in crate::player) fn level_after_mute_toggle(&self) -> f32 {
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
        volume.set_level(volume.level + 0.02);
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

        volume.set_level(volume.level_after_mute_toggle());
        let mut restored = PlaybackVolumeState::new(volume.settings());
        assert_eq!(restored.level, 0.0);
        restored.set_level(restored.level_after_mute_toggle());
        assert_eq!(restored.level, 0.42);
    }
}
