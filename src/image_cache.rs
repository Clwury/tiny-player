use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;

use crate::{
    emby::{EmbyImageRequest, EmbyImageType, ImageQuality},
    server::CachedServer,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CachedImageKey {
    pub server_id: String,
    pub item_id: String,
    pub image_type: EmbyImageType,
    pub tag: String,
    pub max_width: Option<u32>,
    pub quality: ImageQuality,
}

impl CachedImageKey {
    pub fn from_request(server: &CachedServer, request: &EmbyImageRequest) -> Option<Self> {
        let tag = request.tag.as_deref()?.trim();
        if tag.is_empty() {
            return None;
        }

        Some(Self {
            server_id: server.id.clone(),
            item_id: request.item_id.clone(),
            image_type: request.image_type,
            tag: tag.to_string(),
            max_width: request.max_width,
            quality: request.quality,
        })
    }
}

pub fn cached_image_path(key: &CachedImageKey) -> Result<PathBuf> {
    cached_image_path_in(&image_cache_dir()?, key)
}

pub fn cached_image_exists(key: &CachedImageKey) -> Result<Option<PathBuf>> {
    let path = cached_image_path(key)?;
    Ok(path.exists().then_some(path))
}

pub fn write_cached_image(key: &CachedImageKey, bytes: &[u8]) -> Result<PathBuf> {
    let path = cached_image_path(key)?;
    write_cached_image_to(&path, bytes)?;
    Ok(path)
}

fn image_cache_dir() -> Result<PathBuf> {
    let dirs =
        ProjectDirs::from("dev", "tiny", "Tiny").ok_or_else(|| anyhow!("无法定位用户缓存目录"))?;
    Ok(dirs.cache_dir().join("images"))
}

fn cached_image_path_in(base_dir: &Path, key: &CachedImageKey) -> Result<PathBuf> {
    let size = key
        .max_width
        .map(|width| format!("w{width}"))
        .unwrap_or_else(|| "woriginal".to_string());
    let file_name = format!("{}-q{}.img", size, key.quality.get());

    Ok(base_dir
        .join(sanitize_segment(&key.server_id))
        .join(sanitize_segment(&key.item_id))
        .join(key.image_type.as_path_segment())
        .join(sanitize_segment(&key.tag))
        .join(file_name))
}

fn write_cached_image_to(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建图片缓存目录失败：{}", parent.display()))?;
        set_dir_permissions(parent)?;
    }

    let temp_path = path.with_extension("img.tmp");
    fs::write(&temp_path, bytes)
        .with_context(|| format!("写入图片缓存失败：{}", temp_path.display()))?;
    set_file_permissions(&temp_path)?;
    fs::rename(&temp_path, path)
        .with_context(|| format!("保存图片缓存失败：{}", path.display()))?;

    Ok(())
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
        .with_context(|| format!("设置图片缓存目录权限失败：{}", path.display()))
}

#[cfg(not(unix))]
fn set_dir_permissions(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("设置图片缓存文件权限失败：{}", path.display()))
}

#[cfg(not(unix))]
fn set_file_permissions(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{Protocol, ServerEndpoint};

    fn server() -> CachedServer {
        CachedServer {
            id: "server/local".to_string(),
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
            access_token: Some("token".to_string()),
            item_counts: None,
            added_at_unix: 123,
        }
    }

    #[test]
    fn builds_key_from_tagged_request() {
        let request =
            EmbyImageRequest::primary("36089", Some("tag-1".to_string())).with_max_width(640);

        let key = CachedImageKey::from_request(&server(), &request).unwrap();

        assert_eq!(key.server_id, "server/local");
        assert_eq!(key.item_id, "36089");
        assert_eq!(key.image_type, EmbyImageType::Primary);
        assert_eq!(key.tag, "tag-1");
        assert_eq!(key.max_width, Some(640));
        assert_eq!(key.quality, ImageQuality::DEFAULT);
    }

    #[test]
    fn ignores_untagged_request() {
        let request = EmbyImageRequest::primary("36089", None);

        assert!(CachedImageKey::from_request(&server(), &request).is_none());
    }

    #[test]
    fn builds_sanitized_cache_path() {
        let temp = tempfile::tempdir().unwrap();
        let key = CachedImageKey {
            server_id: "server/local".to_string(),
            item_id: "../36089".to_string(),
            image_type: EmbyImageType::Primary,
            tag: "tag/value".to_string(),
            max_width: Some(640),
            quality: ImageQuality::DEFAULT,
        };

        let path = cached_image_path_in(temp.path(), &key).unwrap();

        assert!(path.starts_with(temp.path()));
        assert_eq!(path.file_name().unwrap(), "w640-q90.img");
        assert!(path.to_string_lossy().contains("server_local"));
        assert!(path.to_string_lossy().contains("36089"));
        assert!(path.to_string_lossy().contains("tag_value"));
    }

    #[test]
    fn writes_cached_image_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let key = CachedImageKey {
            server_id: "server-local".to_string(),
            item_id: "36089".to_string(),
            image_type: EmbyImageType::Primary,
            tag: "tag-1".to_string(),
            max_width: Some(640),
            quality: ImageQuality::DEFAULT,
        };
        let path = cached_image_path_in(temp.path(), &key).unwrap();

        write_cached_image_to(&path, b"image-bytes").unwrap();

        assert_eq!(fs::read(path).unwrap(), b"image-bytes");
    }

    #[test]
    fn falls_back_for_empty_sanitized_segments() {
        assert_eq!(sanitize_segment("///"), "unknown");
        assert_eq!(sanitize_segment(".."), "unknown");
    }
}
