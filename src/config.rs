//! Shell-owned global configuration. Server records have a single feature owner;
//! persistence composes both into the unchanged ServerCache document.
use crate::{
    media::{ItemSortPreferences, PlaybackLanguagePreferences},
    search_history::SearchHistory,
    server::feature::ServerCatalog,
    storage::{ServerCache, WindowState},
    theme::ColorTheme,
};
use tiny_playback::{PlaybackCacheConfig, PlaybackVolumeSettings};

#[derive(Clone)]
pub(crate) struct GlobalConfig {
    version: u32,
    pub(crate) device_id: String,
    window: Option<WindowState>,
    pub(crate) playback: PlaybackCacheConfig,
    pub(crate) color_theme: ColorTheme,
    pub(crate) track_languages: PlaybackLanguagePreferences,
    pub(crate) playback_volume: PlaybackVolumeSettings,
    pub(crate) search_history: SearchHistory,
    pub(crate) items_sort: ItemSortPreferences,
}

impl GlobalConfig {
    pub(crate) fn split(cache: ServerCache) -> (Self, ServerCatalog) {
        let ServerCache {
            version,
            device_id,
            window,
            playback,
            color_theme,
            track_languages,
            playback_volume,
            search_history,
            items_sort,
            servers,
            auto_start_server_id,
        } = cache;
        (
            Self {
                version,
                device_id,
                window,
                playback,
                color_theme,
                track_languages,
                playback_volume,
                search_history,
                items_sort,
            },
            ServerCatalog {
                servers,
                auto_start_server_id,
            },
        )
    }

    pub(crate) fn snapshot(&self, catalog: &ServerCatalog) -> ServerCache {
        ServerCache {
            version: self.version,
            device_id: self.device_id.clone(),
            window: self.window.clone(),
            playback: self.playback.clone(),
            color_theme: self.color_theme,
            track_languages: self.track_languages,
            playback_volume: self.playback_volume,
            search_history: self.search_history.clone(),
            items_sort: self.items_sort,
            servers: catalog.servers.clone(),
            auto_start_server_id: catalog.auto_start_server_id.clone(),
        }
    }

    pub(crate) fn set_window_size(&mut self, width: u32, height: u32) -> bool {
        if width == 0 || height == 0 {
            return false;
        }
        let next = WindowState { width, height };
        if self.window.as_ref() == Some(&next) {
            return false;
        }
        self.window = Some(next);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitting_ownership_and_composing_snapshot_preserves_exact_json() {
        let mut cache = ServerCache::empty();
        cache.auto_start_server_id = Some("local".into());
        cache.playback.cache_secs = 42.125;
        cache.playback_volume.level = 0.47;
        cache.items_sort = ItemSortPreferences {
            sort_by: crate::emby::UserItemsSort::PremiereDate,
            sort_order: crate::emby::SortOrder::Descending,
        };
        cache.set_window_size(1234, 789);
        cache.servers.push(
            serde_json::from_value(serde_json::json!({
                "id": "local", "server_id": "remote", "user_id": "user",
                "endpoint": {"protocol":"Https","address":"example.com","port":443,"path":""},
                "username":"test", "password":"", "added_at_unix": 0,
                "item_counts":{"movie_count":12,"series_count":34}
            }))
            .unwrap(),
        );
        let expected = serde_json::to_value(&cache).unwrap();
        let (config, catalog) = GlobalConfig::split(cache);
        assert_eq!(
            serde_json::to_value(config.snapshot(&catalog)).unwrap(),
            expected
        );
    }
}
