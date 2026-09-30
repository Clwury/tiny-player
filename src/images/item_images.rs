//! Blocking cache/network IO, invoked exclusively by background effect runners.
use super::{ImageRepository, cache, controller::ItemImageRequest};
use crate::{emby::EmbyClient, server::CachedServer};
use anyhow::{Result, ensure};
use std::path::{Path, PathBuf};

pub(crate) struct EmbyImageRepository {
    pub(crate) client: EmbyClient,
    pub(crate) server: CachedServer,
}

impl ImageRepository<ItemImageRequest> for EmbyImageRepository {
    type Image = PathBuf;
    fn load(&self, image: &ItemImageRequest) -> Result<PathBuf> {
        self.load_in(&cache::image_cache_dir()?, image)
    }
}

impl EmbyImageRepository {
    fn load_in(&self, directory: &Path, image: &ItemImageRequest) -> Result<PathBuf> {
        ensure!(
            cache::CachedImageKey::from_request(&self.server.id, &image.request).as_ref()
                == Some(&image.key),
            "图片请求与缓存标识不匹配"
        );
        if let Some(path) = cache::cached_image_exists_in(directory, &image.key)? {
            return Ok(path);
        }
        let downloaded = self.client.item_image(&self.server, &image.request)?;
        let path = cache::write_cached_image_in(
            directory,
            &image.key,
            &downloaded.bytes,
            downloaded.content_type.as_deref(),
        )?;
        let _ = cache::prune_cache_dir(directory, cache::DEFAULT_MAX_CACHE_BYTES);
        Ok(path)
    }
}

impl std::fmt::Debug for EmbyImageRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbyImageRepository")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emby::EmbyImageRequest;

    fn repository(url: &str) -> EmbyImageRepository {
        let url = url::Url::parse(url).unwrap();
        let server = serde_json::from_value(serde_json::json!({
            "id":"local", "server_id":"remote", "user_id":"user", "access_token":"test-token",
            "endpoint":{"protocol":"Http", "address":url.host_str().unwrap(), "port":url.port().unwrap(), "path":"/emby"},
            "username":"test", "password":"", "added_at_unix":0
        })).unwrap();
        EmbyImageRepository {
            client: EmbyClient::new("test-device".into()).unwrap(),
            server,
        }
    }

    fn image() -> ItemImageRequest {
        let request = EmbyImageRequest::primary("item", Some("tag".into())).with_max_width(640);
        ItemImageRequest {
            key: cache::CachedImageKey::from_request("local", &request).unwrap(),
            request,
        }
    }

    #[test]
    fn downloads_with_original_query_and_auth_then_reuses_cache_offline() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = include_bytes!("../../assets/icons/emby.png");
        let (url, server) = crate::images::server_icons::tests::mock_icon(200, bytes);
        let repository = repository(&url);
        let image = image();
        let path = repository.load_in(directory.path(), &image).unwrap();
        let request = server.join().unwrap();
        assert!(request.starts_with(
            "GET /emby/Items/item/Images/Primary?maxWidth=640&tag=tag&quality=90 HTTP/1.1\r\n"
        ));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-emby-token: test-token\r\n")
        );
        assert_eq!(
            path,
            directory.path().join("local/item/Primary/tag/w640-q90.png")
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(repository.load_in(directory.path(), &image).unwrap(), path);
        assert!(!path.with_extension("img.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn reads_legacy_cache_extensions_and_rejects_mismatched_keys_before_io() {
        let directory = tempfile::tempdir().unwrap();
        let repository = repository("http://127.0.0.1:9");
        let image = image();
        let path =
            cache::write_cached_image_in(directory.path(), &image.key, b"legacy", None).unwrap();
        assert_eq!(path.extension().unwrap(), "img");
        assert_eq!(repository.load_in(directory.path(), &image).unwrap(), path);
        let mut mismatch = image.clone();
        mismatch.key.server_id = "other".into();
        assert_eq!(
            repository
                .load_in(directory.path(), &mismatch)
                .unwrap_err()
                .to_string(),
            "图片请求与缓存标识不匹配"
        );
        mismatch = image;
        mismatch.key.max_width = Some(800);
        assert_eq!(
            repository
                .load_in(directory.path(), &mismatch)
                .unwrap_err()
                .to_string(),
            "图片请求与缓存标识不匹配"
        );
    }

    #[test]
    fn failed_download_does_not_create_a_cache_entry() {
        let directory = tempfile::tempdir().unwrap();
        let (url, server) = crate::images::server_icons::tests::mock_icon(503, b"unavailable");
        let repository = repository(&url);
        let image = image();
        assert!(repository.load_in(directory.path(), &image).is_err());
        server.join().unwrap();
        assert!(
            cache::cached_image_exists_in(directory.path(), &image.key)
                .unwrap()
                .is_none()
        );
    }
}
