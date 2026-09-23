use super::PlaybackCacheConfig;

/// Supply the application default separately from an explicit user path, so
/// each cache layer's environment override retains its existing precedence.
pub(super) fn engine_cache_config(mut config: PlaybackCacheConfig) -> PlaybackCacheConfig {
    config.fallback_cache_dir = Some(crate::app_metadata::default_playback_cache_dir());
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_cache_default_is_runtime_only_and_preserves_user_settings() {
        for configured_dir in [None, Some(std::path::PathBuf::from("custom-cache"))] {
            let saved = PlaybackCacheConfig {
                cache_dir: configured_dir.clone(),
                ..Default::default()
            };
            let runtime = engine_cache_config(saved.clone());
            assert_eq!(runtime.cache_dir, configured_dir);
            assert_eq!(
                runtime.fallback_cache_dir,
                Some(crate::app_metadata::default_playback_cache_dir())
            );
            assert_eq!(
                serde_json::to_value(&runtime).unwrap(),
                serde_json::to_value(&saved).unwrap()
            );
            let restored: PlaybackCacheConfig =
                serde_json::from_value(serde_json::to_value(&runtime).unwrap()).unwrap();
            assert_eq!(restored, saved);
        }
    }
}
