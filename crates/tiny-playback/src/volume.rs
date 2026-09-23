use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybackVolumeSettings {
    pub level: f32,
    pub unmuted_level: f32,
}

impl Default for PlaybackVolumeSettings {
    fn default() -> Self {
        Self {
            level: 1.0,
            unmuted_level: 1.0,
        }
    }
}

impl PlaybackVolumeSettings {
    pub fn normalized(self) -> Self {
        let level = clamp_playback_volume(self.level);
        let unmuted_level = if level > f32::EPSILON {
            level
        } else if self.unmuted_level.is_finite() && self.unmuted_level > f32::EPSILON {
            self.unmuted_level.min(1.0)
        } else {
            1.0
        };
        Self {
            level,
            unmuted_level,
        }
    }
}

pub fn clamp_playback_volume(volume: f32) -> f32 {
    let volume = if volume.is_finite() { volume } else { 1.0 };
    volume.clamp(0.0, 1.0)
}
