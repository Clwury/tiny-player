use std::{fs, path::Path};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    app_metadata,
    player::{PlaybackCacheConfig, PlaybackLanguagePreferences},
    server::CachedServer,
    theme::ColorTheme,
};

const CACHE_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerCache {
    pub version: u32,
    pub device_id: String,
    pub servers: Vec<CachedServer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    window: Option<WindowState>,
    /// Global playback tuning persisted alongside the server list. The
    /// `default` attribute keeps older cache files backward compatible.
    #[serde(default)]
    pub playback: PlaybackCacheConfig,
    #[serde(default)]
    pub color_theme: ColorTheme,
    #[serde(default)]
    pub track_languages: PlaybackLanguagePreferences,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowState {
    pub width: u32,
    pub height: u32,
}

impl ServerCache {
    pub fn empty() -> Self {
        Self {
            version: CACHE_VERSION,
            device_id: Uuid::new_v4().to_string(),
            servers: Vec::new(),
            window: None,
            playback: PlaybackCacheConfig::default(),
            color_theme: ColorTheme::default(),
            track_languages: PlaybackLanguagePreferences::default(),
        }
    }

    pub fn window_size(&self) -> Option<WindowState> {
        self.window
            .as_ref()
            .filter(|window| window.width > 0 && window.height > 0)
            .cloned()
    }

    pub fn set_window_size(&mut self, width: u32, height: u32) -> bool {
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

pub fn load_or_init() -> Result<ServerCache> {
    load_or_init_from(&cache_path()?)
}

pub fn save(cache: &ServerCache) -> Result<()> {
    save_to(cache, &cache_path()?)
}

pub fn upsert_server(cache: &mut ServerCache, mut server: CachedServer) {
    if let Some(existing) = cache
        .servers
        .iter_mut()
        .find(|existing| same_server(existing, &server))
    {
        server.id = existing.id.clone();
        *existing = server;
    } else {
        cache.servers.push(server);
    }
}

pub fn update_server_by_id(cache: &mut ServerCache, server: CachedServer) -> bool {
    if let Some(existing) = cache
        .servers
        .iter_mut()
        .find(|existing| existing.id == server.id)
    {
        *existing = server;
        true
    } else {
        false
    }
}

pub fn delete_server_by_id(cache: &mut ServerCache, id: &str) -> bool {
    let original_len = cache.servers.len();
    cache.servers.retain(|server| server.id != id);
    cache.servers.len() != original_len
}

fn same_server(a: &CachedServer, b: &CachedServer) -> bool {
    if a.server_id.is_some()
        && a.server_id == b.server_id
        && a.user_id.is_some()
        && a.user_id == b.user_id
    {
        return true;
    }

    a.endpoint == b.endpoint && a.username == b.username
}

fn cache_path() -> Result<std::path::PathBuf> {
    Ok(app_metadata::config_dir()?.join("servers.json"))
}

pub(crate) fn load_or_init_from(path: &Path) -> Result<ServerCache> {
    if !path.exists() {
        return Ok(ServerCache::empty());
    }

    let bytes =
        fs::read(path).with_context(|| format!("读取服务器缓存失败：{}", path.display()))?;
    let mut cache: ServerCache = serde_json::from_slice(&bytes)
        .with_context(|| format!("解析服务器缓存失败：{}", path.display()))?;

    if cache.device_id.is_empty() {
        cache.device_id = Uuid::new_v4().to_string();
    }
    // Normalize both newly added and hand-edited settings before they reach
    // the playback worker. This also makes old cache files (without a
    // `playback` field) behave exactly like a fresh configuration.
    cache.playback = cache.playback.clone().normalized();

    Ok(cache)
}

pub(crate) fn save_to(cache: &ServerCache, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建服务器缓存目录失败：{}", parent.display()))?;
        set_dir_permissions(parent)?;
    }

    let temp_path = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(cache).context("序列化服务器缓存失败")?;
    fs::write(&temp_path, bytes)
        .with_context(|| format!("写入服务器缓存失败：{}", temp_path.display()))?;
    set_file_permissions(&temp_path)?;
    fs::rename(&temp_path, path)
        .with_context(|| format!("保存服务器缓存失败：{}", path.display()))?;

    Ok(())
}

#[cfg(unix)]
fn set_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("设置服务器缓存目录权限失败：{}", path.display()))
}

#[cfg(not(unix))]
fn set_dir_permissions(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("设置服务器缓存文件权限失败：{}", path.display()))
}

#[cfg(not(unix))]
fn set_file_permissions(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{CachedItemCounts, Protocol, ServerEndpoint};

    fn server(id: &str, token: &str) -> CachedServer {
        CachedServer {
            id: id.to_string(),
            endpoint: ServerEndpoint {
                protocol: Protocol::Https,
                address: "example.com".to_string(),
                port: 443,
                path: String::new(),
            },
            username: "luv".to_string(),
            password: "secret".to_string(),
            user_id: Some("user-1".to_string()),
            server_id: Some("server-1".to_string()),
            server_name: Some("Home".to_string()),
            access_token: Some(token.to_string()),
            item_counts: None,
            added_at_unix: 123,
        }
    }

    #[test]
    fn initializes_missing_cache() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");

        let cache = load_or_init_from(&path).unwrap();

        assert_eq!(cache.version, CACHE_VERSION);
        assert!(!cache.device_id.is_empty());
        assert!(cache.servers.is_empty());
    }

    #[test]
    fn saves_and_loads_cache() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let mut cache = ServerCache::empty();
        cache.servers.push(server("server-local", "token"));

        save_to(&cache, &path).unwrap();
        let loaded = load_or_init_from(&path).unwrap();

        assert_eq!(loaded.device_id, cache.device_id);
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(loaded.servers[0].access_token.as_deref(), Some("token"));
    }

    #[test]
    fn saves_and_loads_playback_settings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let mut cache = ServerCache::empty();
        cache.playback.cache_secs = 42.5;
        cache.playback.adaptive_readahead = false;
        cache.playback.total_cache_max_bytes = 96 * 1024 * 1024;

        save_to(&cache, &path).unwrap();
        let loaded = load_or_init_from(&path).unwrap();

        assert_eq!(loaded.playback.cache_secs, 42.5);
        assert!(!loaded.playback.adaptive_readahead);
        assert_eq!(loaded.playback.total_cache_max_bytes, 96 * 1024 * 1024);
    }

    #[test]
    fn saves_and_restores_color_theme() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let mut cache = ServerCache::empty();
        for selection in ColorTheme::ALL {
            cache.color_theme = selection;
            save_to(&cache, &path).unwrap();
            assert_eq!(load_or_init_from(&path).unwrap().color_theme, selection);
        }
    }

    #[test]
    fn saves_and_restores_track_languages() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let mut cache = ServerCache::empty();
        for language in crate::player::TrackLanguage::ALL {
            cache.track_languages.audio = language;
            cache.track_languages.subtitle = language;
            save_to(&cache, &path).unwrap();
            assert_eq!(
                load_or_init_from(&path).unwrap().track_languages,
                cache.track_languages
            );
        }
    }

    #[test]
    fn missing_or_unknown_track_languages_preserve_other_settings() {
        let mut json = serde_json::to_value(ServerCache::empty()).unwrap();
        json["device_id"] = "existing-device".into();
        json["track_languages"] = serde_json::json!({"audio": "unknown-language"});
        let cache: ServerCache = serde_json::from_value(json).unwrap();
        assert_eq!(
            cache.track_languages,
            PlaybackLanguagePreferences::default()
        );
        assert_eq!(cache.device_id, "existing-device");
    }

    #[test]
    fn unknown_color_theme_falls_back_without_discarding_server_settings() {
        let mut json = serde_json::to_value(ServerCache::empty()).unwrap();
        json["color_theme"] = "removed-theme".into();
        json["device_id"] = "existing-device".into();
        let cache: ServerCache = serde_json::from_value(json).unwrap();
        assert_eq!(cache.color_theme, ColorTheme::Mocha);
        assert_eq!(cache.device_id, "existing-device");
    }

    #[test]
    fn loads_cache_without_window_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        fs::write(
            &path,
            r#"{"version":1,"device_id":"device-local","servers":[]}"#,
        )
        .unwrap();

        let loaded = load_or_init_from(&path).unwrap();

        assert_eq!(loaded.device_id, "device-local");
        assert_eq!(loaded.window_size(), None);
        assert_eq!(loaded.playback, PlaybackCacheConfig::default());
        assert_eq!(loaded.color_theme, ColorTheme::Mocha);
        assert_eq!(
            loaded.track_languages,
            PlaybackLanguagePreferences::default()
        );
    }

    #[test]
    fn saves_and_loads_window_size() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let mut cache = ServerCache::empty();

        assert!(cache.set_window_size(1280, 800));
        save_to(&cache, &path).unwrap();
        let loaded = load_or_init_from(&path).unwrap();

        assert_eq!(
            loaded.window_size(),
            Some(WindowState {
                width: 1280,
                height: 800,
            })
        );
    }

    #[test]
    fn saves_and_loads_item_counts() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("servers.json");
        let mut cache = ServerCache::empty();
        let mut server = server("server-local", "token");
        server.item_counts = Some(CachedItemCounts {
            movie_count: 16417,
            series_count: 16395,
        });
        cache.servers.push(server);

        save_to(&cache, &path).unwrap();
        let loaded = load_or_init_from(&path).unwrap();

        assert_eq!(
            loaded.servers[0].item_counts,
            Some(CachedItemCounts {
                movie_count: 16417,
                series_count: 16395,
            })
        );
    }

    #[test]
    fn upserts_same_server_and_user() {
        let mut cache = ServerCache::empty();
        upsert_server(&mut cache, server("first", "old"));
        upsert_server(&mut cache, server("second", "new"));

        assert_eq!(cache.servers.len(), 1);
        assert_eq!(cache.servers[0].id, "first");
        assert_eq!(cache.servers[0].access_token.as_deref(), Some("new"));
    }

    #[test]
    fn updates_server_by_id() {
        let mut cache = ServerCache::empty();
        cache.servers.push(server("server-local", "old"));
        let mut updated = server("server-local", "new");
        updated.endpoint.address = "updated.example.com".to_string();
        updated.username = "new-user".to_string();
        updated.password = "new-secret".to_string();

        assert!(update_server_by_id(&mut cache, updated));

        assert_eq!(cache.servers.len(), 1);
        assert_eq!(cache.servers[0].id, "server-local");
        assert_eq!(cache.servers[0].endpoint.address, "updated.example.com");
        assert_eq!(cache.servers[0].username, "new-user");
        assert_eq!(cache.servers[0].password, "new-secret");
        assert_eq!(cache.servers[0].access_token.as_deref(), Some("new"));
    }

    #[test]
    fn update_missing_server_by_id_returns_false() {
        let mut cache = ServerCache::empty();
        cache.servers.push(server("server-local", "old"));

        assert!(!update_server_by_id(&mut cache, server("missing", "new")));

        assert_eq!(cache.servers.len(), 1);
        assert_eq!(cache.servers[0].id, "server-local");
        assert_eq!(cache.servers[0].access_token.as_deref(), Some("old"));
    }

    #[test]
    fn deletes_server_by_id() {
        let mut cache = ServerCache::empty();
        cache.servers.push(server("first", "old"));
        let mut second = server("second", "new");
        second.endpoint.address = "other.example.com".to_string();
        cache.servers.push(second);

        assert!(delete_server_by_id(&mut cache, "first"));

        assert_eq!(cache.servers.len(), 1);
        assert_eq!(cache.servers[0].id, "second");
    }

    #[test]
    fn delete_missing_server_by_id_returns_false() {
        let mut cache = ServerCache::empty();
        cache.servers.push(server("server-local", "old"));

        assert!(!delete_server_by_id(&mut cache, "missing"));

        assert_eq!(cache.servers.len(), 1);
        assert_eq!(cache.servers[0].id, "server-local");
    }
}
