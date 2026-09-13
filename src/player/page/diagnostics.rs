//! Activity/cache diagnostics default to debug builds. Set TINY_DEV_PLAYER to
//! 1/true/on or 0/false/off before launching to override either build profile.

use std::sync::OnceLock;

pub(super) fn playback_diagnostics_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        resolve_playback_diagnostics(
            cfg!(debug_assertions),
            std::env::var("TINY_DEV_PLAYER").ok().as_deref(),
        )
    })
}

fn resolve_playback_diagnostics(debug_build: bool, override_value: Option<&str>) -> bool {
    match override_value
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("1" | "true" | "on") => true,
        Some("0" | "false" | "off") => false,
        _ => debug_build,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_follow_build_defaults_without_a_valid_override() {
        for value in [None, Some(""), Some(" "), Some("invalid")] {
            assert!(resolve_playback_diagnostics(true, value));
            assert!(!resolve_playback_diagnostics(false, value));
        }
    }

    #[test]
    fn environment_can_enable_release_diagnostics_and_disable_debug_diagnostics() {
        for debug_build in [false, true] {
            for value in ["1", "true", "on", " TRUE ", "On"] {
                assert!(resolve_playback_diagnostics(debug_build, Some(value)));
            }
            for value in ["0", "false", "off", " FALSE ", "Off"] {
                assert!(!resolve_playback_diagnostics(debug_build, Some(value)));
            }
        }
    }
}
