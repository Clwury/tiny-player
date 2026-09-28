use serde::{Deserialize, Serialize};

/// Video decoder selection, applied when a playback input is opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HardwareDecodeMode {
    /// Decode video on the CPU.
    Off,
    /// Prefer Vulkan hardware decoding, allowing software fallback.
    #[default]
    Auto,
    /// Require Vulkan hardware decoding and report failures without fallback.
    ForceVulkan,
}

impl HardwareDecodeMode {
    pub(crate) fn with_env_override(self) -> Self {
        self.resolve(std::env::var("TINY_HWDEC").ok().as_deref())
    }

    fn resolve(self, value: Option<&str>) -> Self {
        value.and_then(Self::parse).unwrap_or(self)
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "0" | "off" | "false" | "no" | "disabled" | "software" | "sw" => Some(Self::Off),
            "1" | "on" | "true" | "yes" | "auto" | "vulkan-auto" => Some(Self::Auto),
            "vulkan" | "force" | "force-vulkan" | "vulkan-force" => Some(Self::ForceVulkan),
            _ => None,
        }
    }

    pub(crate) fn should_try_vulkan(self) -> bool {
        matches!(self, Self::Auto | Self::ForceVulkan)
    }

    pub(crate) fn allows_fallback(self) -> bool {
        matches!(self, Self::Auto)
    }
}

#[cfg(test)]
mod tests {
    use super::HardwareDecodeMode;
    use crate::PlaybackCacheConfig;

    #[test]
    fn hardware_decode_preferences_survive_config_roundtrip_and_normalization() {
        let legacy: PlaybackCacheConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.hardware_decode, HardwareDecodeMode::Auto);
        for (mode, value) in [
            (HardwareDecodeMode::Auto, "auto"),
            (HardwareDecodeMode::Off, "off"),
            (HardwareDecodeMode::ForceVulkan, "force_vulkan"),
        ] {
            let config = PlaybackCacheConfig {
                hardware_decode: mode,
                ..PlaybackCacheConfig::default()
            };
            let json = serde_json::to_value(&config).unwrap();
            assert_eq!(json["hardware_decode"], value);
            let restored: PlaybackCacheConfig = serde_json::from_value(json).unwrap();
            assert_eq!(restored.normalized(), config);
        }
    }

    #[test]
    fn hardware_decode_environment_overrides_only_valid_preferences() {
        for mode in [
            HardwareDecodeMode::Auto,
            HardwareDecodeMode::Off,
            HardwareDecodeMode::ForceVulkan,
        ] {
            assert_eq!(mode.resolve(None), mode);
            assert_eq!(mode.resolve(Some("unsupported")), mode);
            assert_eq!(mode.resolve(Some(" AUTO ")), HardwareDecodeMode::Auto);
            assert_eq!(mode.resolve(Some("off")), HardwareDecodeMode::Off);
            assert_eq!(mode.resolve(Some("")), HardwareDecodeMode::Off);
            assert_eq!(
                mode.resolve(Some("force-vulkan")),
                HardwareDecodeMode::ForceVulkan
            );
        }
    }
}
