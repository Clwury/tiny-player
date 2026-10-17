use serde::{Deserialize, Serialize};

/// Hardware backend attached to the video decoder, independent of user preferences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HardwareDecodeBackend {
    Vulkan,
    /// VA-API decoding with frames downloaded to system memory.
    Vaapi,
    /// VA-API decoding with DMA-BUF frames mapped directly to Vulkan.
    VaapiVulkan,
}

/// Video decoder selection, applied when a playback input is opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HardwareDecodeMode {
    /// Decode video on the CPU.
    Off,
    /// Try the platform and GPU's preferred GPU frame paths, then software decoding.
    #[default]
    Auto,
    /// Development Auto policy, also allowing VA-API system-memory copyback on Linux.
    AutoWithCopyback,
    /// Require Vulkan hardware decoding and report failures without fallback.
    ForceVulkan,
    /// Require VA-API decoding on Linux, downloading frames for video processing.
    ForceVaapi,
    /// Require VA-API decoding and direct Vulkan mapping on Linux.
    ForceVaapiVulkan,
}

impl HardwareDecodeBackend {
    pub(crate) fn forced_mode(self) -> HardwareDecodeMode {
        match self {
            Self::Vulkan => HardwareDecodeMode::ForceVulkan,
            Self::Vaapi => HardwareDecodeMode::ForceVaapi,
            Self::VaapiVulkan => HardwareDecodeMode::ForceVaapiVulkan,
        }
    }
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
            "auto-copy" | "auto-with-copyback" | "auto_with_copyback" => {
                Some(Self::AutoWithCopyback)
            }
            "vulkan" | "force" | "force-vulkan" | "vulkan-force" => Some(Self::ForceVulkan),
            "vaapi" | "force-vaapi" | "vaapi-force" | "vaapi-copy" => Some(Self::ForceVaapi),
            "vaapi-vulkan" | "force-vaapi-vulkan" | "vaapi-gpu" => Some(Self::ForceVaapiVulkan),
            _ => None,
        }
    }

    pub(crate) fn should_try_hardware(self) -> bool {
        self != Self::Off
    }

    pub(crate) fn uses_vaapi(self) -> bool {
        matches!(self, Self::ForceVaapi | Self::ForceVaapiVulkan)
    }

    pub(crate) fn allows_fallback(self) -> bool {
        matches!(self, Self::Auto | Self::AutoWithCopyback)
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
            (HardwareDecodeMode::AutoWithCopyback, "auto_with_copyback"),
            (HardwareDecodeMode::Off, "off"),
            (HardwareDecodeMode::ForceVulkan, "force_vulkan"),
            (HardwareDecodeMode::ForceVaapi, "force_vaapi"),
            (HardwareDecodeMode::ForceVaapiVulkan, "force_vaapi_vulkan"),
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
            HardwareDecodeMode::AutoWithCopyback,
            HardwareDecodeMode::Off,
            HardwareDecodeMode::ForceVulkan,
            HardwareDecodeMode::ForceVaapi,
            HardwareDecodeMode::ForceVaapiVulkan,
        ] {
            assert_eq!(mode.resolve(None), mode);
            assert_eq!(mode.resolve(Some("unsupported")), mode);
            assert_eq!(mode.resolve(Some(" AUTO ")), HardwareDecodeMode::Auto);
            for value in ["auto-copy", "auto-with-copyback", "auto_with_copyback"] {
                assert_eq!(
                    mode.resolve(Some(value)),
                    HardwareDecodeMode::AutoWithCopyback
                );
            }
            assert_eq!(mode.resolve(Some("off")), HardwareDecodeMode::Off);
            assert_eq!(mode.resolve(Some("")), HardwareDecodeMode::Off);
            assert_eq!(
                mode.resolve(Some("force-vulkan")),
                HardwareDecodeMode::ForceVulkan
            );
            assert_eq!(mode.resolve(Some("vaapi")), HardwareDecodeMode::ForceVaapi);
            for value in ["vaapi-vulkan", "force-vaapi-vulkan", "vaapi-gpu"] {
                assert_eq!(
                    mode.resolve(Some(value)),
                    HardwareDecodeMode::ForceVaapiVulkan
                );
            }
        }
    }
}
