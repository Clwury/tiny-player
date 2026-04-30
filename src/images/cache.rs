use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result, anyhow};
use directories::ProjectDirs;

use crate::{
    emby::{EmbyImageRequest, EmbyImageType, ImageQuality},
    server::CachedServer,
};

pub const DEFAULT_MAX_CACHE_BYTES: u64 = 512 * 1024 * 1024;

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

pub fn cached_image_exists(key: &CachedImageKey) -> Result<Option<PathBuf>> {
    for path in cached_image_candidate_paths(key)? {
        if path.exists() {
            return Ok(Some(path));
        }
    }

    Ok(None)
}

pub fn write_cached_image(
    key: &CachedImageKey,
    bytes: &[u8],
    content_type: Option<&str>,
) -> Result<PathBuf> {
    let path = cached_image_path_for_response(key, bytes, content_type)?;
    write_cached_image_to(&path, bytes)?;
    Ok(path)
}

pub fn prune_cache(max_bytes: u64) -> Result<()> {
    prune_cache_dir(&image_cache_dir()?, max_bytes)
}

fn image_cache_dir() -> Result<PathBuf> {
    let dirs =
        ProjectDirs::from("dev", "tiny", "Tiny").ok_or_else(|| anyhow!("无法定位用户缓存目录"))?;
    Ok(dirs.cache_dir().join("images"))
}

#[cfg(test)]
fn cached_image_path_in(base_dir: &Path, key: &CachedImageKey) -> Result<PathBuf> {
    cached_image_path_with_extension_in(base_dir, key, "img")
}

fn cached_image_path_for_response(
    key: &CachedImageKey,
    bytes: &[u8],
    content_type: Option<&str>,
) -> Result<PathBuf> {
    cached_image_path_with_extension_in(
        &image_cache_dir()?,
        key,
        image_extension_for_response(bytes, content_type),
    )
}

fn cached_image_candidate_paths(key: &CachedImageKey) -> Result<Vec<PathBuf>> {
    let base_dir = image_cache_dir()?;
    IMAGE_CACHE_EXTENSIONS
        .iter()
        .map(|extension| cached_image_path_with_extension_in(&base_dir, key, extension))
        .collect::<Result<Vec<_>>>()
}

const IMAGE_CACHE_EXTENSIONS: &[&str] = &["jpg", "png", "webp", "gif", "img"];

fn cached_image_path_with_extension_in(
    base_dir: &Path,
    key: &CachedImageKey,
    extension: &str,
) -> Result<PathBuf> {
    let size = key
        .max_width
        .map(|width| format!("w{width}"))
        .unwrap_or_else(|| "woriginal".to_string());
    let file_name = format!("{}-q{}.{}", size, key.quality.get(), extension);

    Ok(base_dir
        .join(sanitize_segment(&key.server_id))
        .join(sanitize_segment(&key.item_id))
        .join(key.image_type.as_path_segment())
        .join(sanitize_segment(&key.tag))
        .join(file_name))
}

fn image_extension_for_response(bytes: &[u8], content_type: Option<&str>) -> &'static str {
    content_type
        .and_then(image_extension_for_content_type)
        .unwrap_or_else(|| image_extension_from_bytes(bytes))
}

fn image_extension_for_content_type(content_type: &str) -> Option<&'static str> {
    let mime_type = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase();

    match mime_type.as_str() {
        "image/jpeg" | "image/jpg" | "image/pjpeg" => Some("jpg"),
        "image/png" | "image/x-png" => Some("png"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        _ => None,
    }
}

fn image_extension_from_bytes(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "jpg"
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "gif"
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "webp"
    } else {
        "img"
    }
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

#[derive(Clone, Debug)]
struct CacheFile {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

fn prune_cache_dir(base_dir: &Path, max_bytes: u64) -> Result<()> {
    if !base_dir.exists() {
        return Ok(());
    }

    let mut files = Vec::new();
    collect_cache_files(base_dir, &mut files)?;
    let mut total_bytes = files.iter().map(|file| file.size).sum::<u64>();
    if total_bytes <= max_bytes {
        return Ok(());
    }

    files.sort_by_key(|file| file.modified);
    for file in files {
        if total_bytes <= max_bytes {
            break;
        }

        match fs::remove_file(&file.path) {
            Ok(()) => total_bytes = total_bytes.saturating_sub(file.size),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                total_bytes = total_bytes.saturating_sub(file.size)
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("清理图片缓存失败：{}", file.path.display()));
            }
        }
    }

    Ok(())
}

fn collect_cache_files(dir: &Path, files: &mut Vec<CacheFile>) -> Result<()> {
    for entry in
        fs::read_dir(dir).with_context(|| format!("读取图片缓存目录失败：{}", dir.display()))?
    {
        let entry = entry.with_context(|| format!("读取图片缓存条目失败：{}", dir.display()))?;
        let path = entry.path();
        let metadata = entry
            .metadata()
            .with_context(|| format!("读取图片缓存元数据失败：{}", path.display()))?;
        if metadata.is_dir() {
            collect_cache_files(&path, files)?;
        } else if metadata.is_file() {
            files.push(CacheFile {
                path,
                size: metadata.len(),
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            });
        }
    }

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
    fn detects_common_image_extensions_from_bytes() {
        assert_eq!(
            image_extension_for_response(&[0xff, 0xd8, 0xff, 0xe0], None),
            "jpg"
        );
        assert_eq!(
            image_extension_for_response(b"\x89PNG\r\n\x1a\nrest", None),
            "png"
        );
        assert_eq!(image_extension_for_response(b"GIF89arest", None), "gif");
        assert_eq!(
            image_extension_for_response(b"RIFF\x00\x00\x00\x00WEBPrest", None),
            "webp"
        );
        assert_eq!(image_extension_for_response(b"image-bytes", None), "img");
    }

    #[test]
    fn prefers_content_type_for_image_extension() {
        assert_eq!(
            image_extension_for_response(b"image-bytes", Some("image/jpeg")),
            "jpg"
        );
        assert_eq!(
            image_extension_for_response(b"image-bytes", Some("image/png; charset=utf-8")),
            "png"
        );
        assert_eq!(
            image_extension_for_response(b"image-bytes", Some("IMAGE/WEBP")),
            "webp"
        );
        assert_eq!(
            image_extension_for_response(b"image-bytes", Some("image/gif")),
            "gif"
        );
    }

    #[test]
    fn falls_back_to_bytes_for_unknown_content_type() {
        assert_eq!(
            image_extension_for_response(
                b"\x89PNG\r\n\x1a\nrest",
                Some("application/octet-stream")
            ),
            "png"
        );
    }

    #[test]
    fn builds_extension_specific_cache_path() {
        let temp = tempfile::tempdir().unwrap();
        let key = CachedImageKey {
            server_id: "server-local".to_string(),
            item_id: "36089".to_string(),
            image_type: EmbyImageType::Primary,
            tag: "tag-1".to_string(),
            max_width: Some(640),
            quality: ImageQuality::DEFAULT,
        };

        let path = cached_image_path_with_extension_in(temp.path(), &key, "png").unwrap();

        assert_eq!(path.file_name().unwrap(), "w640-q90.png");
    }

    #[test]
    fn prunes_cache_dir_to_requested_size() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("server/item/Primary/tag");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("old.img"), b"1234").unwrap();
        fs::write(nested.join("older.png"), b"5678").unwrap();
        fs::write(nested.join("new.webp"), b"abcd").unwrap();

        prune_cache_dir(temp.path(), 5).unwrap();

        let mut files = Vec::new();
        collect_cache_files(temp.path(), &mut files).unwrap();
        assert!(files.iter().map(|file| file.size).sum::<u64>() <= 5);
    }

    #[test]
    fn falls_back_for_empty_sanitized_segments() {
        assert_eq!(sanitize_segment("///"), "unknown");
        assert_eq!(sanitize_segment(".."), "unknown");
    }
}
