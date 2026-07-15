use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::{
    emby::{ResumeItems, UserItems, UserViews},
    server::CachedServer,
};

const HOME_SNAPSHOT_VERSION: u32 = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct HomeSnapshot {
    version: u32,
    server_id: String,
    remote_server_id: Option<String>,
    user_id: Option<String>,
    saved_at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) user_views: Option<CachedSection<UserViews>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) resume_items: Option<CachedSection<ResumeItems>>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(super) latest_items_by_view: HashMap<String, CachedSection<UserItems>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct CachedSection<T> {
    pub(super) saved_at_unix: u64,
    pub(super) data: T,
}

impl HomeSnapshot {
    pub(super) fn new(
        server: &CachedServer,
        user_views: Option<UserViews>,
        resume_items: Option<ResumeItems>,
        latest_items_by_view: HashMap<String, UserItems>,
    ) -> Self {
        let saved_at_unix = current_unix_time();
        Self {
            version: HOME_SNAPSHOT_VERSION,
            server_id: server.id.clone(),
            remote_server_id: server.server_id.clone(),
            user_id: server.user_id.clone(),
            saved_at_unix,
            user_views: user_views.map(|data| CachedSection::new(data, saved_at_unix)),
            resume_items: resume_items.map(|data| CachedSection::new(data, saved_at_unix)),
            latest_items_by_view: latest_items_by_view
                .into_iter()
                .map(|(view_id, data)| (view_id, CachedSection::new(data, saved_at_unix)))
                .collect(),
        }
    }

    fn matches_server(&self, server: &CachedServer) -> bool {
        self.version == HOME_SNAPSHOT_VERSION
            && self.server_id == server.id
            && self.remote_server_id == server.server_id
            && self.user_id == server.user_id
    }
}

impl<T> CachedSection<T> {
    fn new(data: T, saved_at_unix: u64) -> Self {
        Self {
            saved_at_unix,
            data,
        }
    }
}

pub(super) fn load_snapshot(server: &CachedServer) -> Result<Option<HomeSnapshot>> {
    load_snapshot_from(&snapshot_path(server)?, server)
}

pub(super) fn save_snapshot(server: &CachedServer, snapshot: &HomeSnapshot) -> Result<()> {
    if !snapshot.matches_server(server) {
        return Err(anyhow!("首页缓存与当前服务器不匹配"));
    }

    save_snapshot_to(&snapshot_path(server)?, snapshot)
}

fn snapshot_path(server: &CachedServer) -> Result<PathBuf> {
    let dirs =
        ProjectDirs::from("dev", "tiny", "Tiny").ok_or_else(|| anyhow!("无法定位用户缓存目录"))?;
    let account = server
        .user_id
        .as_deref()
        .filter(|user_id| !user_id.trim().is_empty())
        .unwrap_or(server.username.as_str());

    Ok(dirs
        .cache_dir()
        .join("home")
        .join(sanitize_segment(&server.id))
        .join(sanitize_segment(account))
        .join("snapshot.json"))
}

fn load_snapshot_from(path: &Path, server: &CachedServer) -> Result<Option<HomeSnapshot>> {
    if !path.exists() {
        return Ok(None);
    }

    let bytes = fs::read(path).with_context(|| format!("读取首页缓存失败：{}", path.display()))?;
    let snapshot: HomeSnapshot = serde_json::from_slice(&bytes)
        .with_context(|| format!("解析首页缓存失败：{}", path.display()))?;

    if snapshot.matches_server(server) {
        Ok(Some(snapshot))
    } else {
        Ok(None)
    }
}

fn save_snapshot_to(path: &Path, snapshot: &HomeSnapshot) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建首页缓存目录失败：{}", parent.display()))?;
        set_dir_permissions(parent)?;
    }

    let temp_path = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(snapshot).context("序列化首页缓存失败")?;
    fs::write(&temp_path, bytes)
        .with_context(|| format!("写入首页缓存失败：{}", temp_path.display()))?;
    set_file_permissions(&temp_path)?;
    fs::rename(&temp_path, path)
        .with_context(|| format!("保存首页缓存失败：{}", path.display()))?;

    Ok(())
}

fn current_unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn sanitize_segment(segment: &str) -> String {
    let sanitized = segment
        .chars()
        .map(|ch| match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '-' | '.' => ch,
            _ => '_',
        })
        .collect::<String>();
    let sanitized = sanitized.trim_matches('.').trim_matches('_');

    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized.to_string()
    }
}

#[cfg(unix)]
fn set_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("设置首页缓存目录权限失败：{}", path.display()))
}

#[cfg(not(unix))]
fn set_dir_permissions(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("设置首页缓存文件权限失败：{}", path.display()))
}

#[cfg(not(unix))]
fn set_file_permissions(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::{
        emby::{ResumeItems, UserItems, UserView, UserViews},
        server::{CachedServer, Protocol, ServerEndpoint},
    };

    use super::*;

    fn server(id: &str, user_id: Option<&str>) -> CachedServer {
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
            user_id: user_id.map(ToString::to_string),
            server_id: Some("remote-server-1".to_string()),
            server_name: Some("Home".to_string()),
            access_token: Some("token".to_string()),
            item_counts: None,
            added_at_unix: 123,
        }
    }

    fn user_views() -> UserViews {
        UserViews {
            items: vec![UserView {
                id: "movies".to_string(),
                name: "电影".to_string(),
                server_id: Some("remote-server-1".to_string()),
                item_type: Some("CollectionFolder".to_string()),
                collection_type: Some("movies".to_string()),
                primary_image_aspect_ratio: None,
                image_tags: None,
            }],
            total_record_count: 1,
        }
    }

    #[test]
    fn saves_and_loads_home_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("snapshot.json");
        let server = server("server-local", Some("user-1"));
        let mut rows = HashMap::new();
        rows.insert(
            "movies".to_string(),
            UserItems {
                items: Vec::new(),
                total_record_count: 0,
            },
        );
        let snapshot = HomeSnapshot::new(
            &server,
            Some(user_views()),
            Some(ResumeItems {
                items: Vec::new(),
                total_record_count: 0,
            }),
            rows,
        );

        save_snapshot_to(&path, &snapshot).unwrap();
        let loaded = load_snapshot_from(&path, &server).unwrap().unwrap();

        assert_eq!(loaded.version, HOME_SNAPSHOT_VERSION);
        assert_eq!(loaded.server_id, "server-local");
        assert_eq!(
            loaded.user_views.as_ref().unwrap().saved_at_unix,
            loaded.saved_at_unix
        );
        assert_eq!(
            loaded.resume_items.as_ref().unwrap().saved_at_unix,
            loaded.saved_at_unix
        );
        assert_eq!(
            loaded
                .latest_items_by_view
                .get("movies")
                .unwrap()
                .saved_at_unix,
            loaded.saved_at_unix
        );
        assert_eq!(
            loaded.user_views.unwrap().data.items[0].name,
            "电影".to_string()
        );
        assert!(loaded.resume_items.is_some());
        assert!(loaded.latest_items_by_view.contains_key("movies"));
    }

    #[test]
    fn replaces_existing_snapshot_without_leaving_a_temp_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("account").join("snapshot.json");
        let server = server("server-local", Some("user-1"));
        let first = HomeSnapshot::new(&server, Some(user_views()), None, HashMap::new());
        let second = HomeSnapshot::new(&server, None, None, HashMap::new());

        save_snapshot_to(&path, &first).unwrap();
        save_snapshot_to(&path, &second).unwrap();

        let loaded = load_snapshot_from(&path, &server).unwrap().unwrap();
        assert!(loaded.user_views.is_none());
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_account_directory_and_file_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("account").join("snapshot.json");
        let server = server("server-local", Some("user-1"));
        let snapshot = HomeSnapshot::new(&server, None, None, HashMap::new());

        save_snapshot_to(&path, &snapshot).unwrap();

        let directory_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[test]
    fn ignores_snapshot_for_different_user() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("snapshot.json");
        let original_server = server("server-local", Some("user-1"));
        let other_user_server = server("server-local", Some("user-2"));
        let snapshot =
            HomeSnapshot::new(&original_server, Some(user_views()), None, HashMap::new());

        save_snapshot_to(&path, &snapshot).unwrap();

        assert!(
            load_snapshot_from(&path, &other_user_server)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn ignores_snapshot_for_different_local_or_remote_server() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("snapshot.json");
        let original = server("server-local", Some("user-1"));
        let different_local = server("server-other", Some("user-1"));
        let mut different_remote = server("server-local", Some("user-1"));
        different_remote.server_id = Some("remote-server-2".to_string());
        let snapshot = HomeSnapshot::new(&original, None, None, HashMap::new());
        save_snapshot_to(&path, &snapshot).unwrap();

        assert!(
            load_snapshot_from(&path, &different_local)
                .unwrap()
                .is_none()
        );
        assert!(
            load_snapshot_from(&path, &different_remote)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn sanitizes_snapshot_path_segments() {
        assert_eq!(sanitize_segment("server/local"), "server_local");
        assert_eq!(sanitize_segment(".."), "unknown");
        assert_eq!(sanitize_segment("///"), "unknown");
    }

    #[test]
    fn ignores_v1_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("snapshot.json");
        let server = server("server-local", Some("user-1"));
        let snapshot = serde_json::json!({
            "version": 1,
            "server_id": "server-local",
            "remote_server_id": "remote-server-1",
            "user_id": "user-1",
            "saved_at_unix": 1,
            "user_view_items": {}
        });
        fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        assert!(load_snapshot_from(&path, &server).unwrap().is_none());
    }

    #[test]
    fn ignores_unknown_snapshot_version() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("snapshot.json");
        let server = server("server-local", Some("user-1"));
        let snapshot = serde_json::json!({
            "version": 99,
            "server_id": "server-local",
            "remote_server_id": "remote-server-1",
            "user_id": "user-1",
            "saved_at_unix": 1
        });
        fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

        assert!(load_snapshot_from(&path, &server).unwrap().is_none());
    }
}
