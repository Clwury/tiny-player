use anyhow::{Context, Result, anyhow, bail};
use reqwest::Method;
use serde::Deserialize;
use tracing::instrument;
use url::Url;

use crate::{player::device_profile, server::CachedServer};

use super::{EmbyClient, api_url};

impl EmbyClient {
    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url(), item_id = %item_id, media_source_id = %media_source_id))]
    pub fn playback_info(
        &self,
        server: &CachedServer,
        item_id: &str,
        media_source_id: &str,
    ) -> Result<PlaybackInfo> {
        validate_item_id(item_id)?;
        validate_media_source_id(media_source_id)?;
        let user_id = authenticated_user_id(server)?;
        let mut url = api_url(&server.endpoint, &["Items", item_id, "PlaybackInfo"])?;
        add_playback_info_query(&mut url, media_source_id, user_id);
        self.send_authenticated_json_body_url(
            server,
            Method::POST,
            url,
            &device_profile(),
            "解析 Emby 播放信息响应失败",
        )
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackInfo {
    #[serde(default)]
    pub media_sources: Vec<PlaybackMediaSource>,
    pub play_session_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackMediaSource {
    pub id: Option<String>,
    pub direct_stream_url: Option<String>,
}

impl PlaybackInfo {
    pub fn direct_stream_source_for(&self, media_source_id: &str) -> Result<&PlaybackMediaSource> {
        validate_media_source_id(media_source_id)?;
        if self.media_sources.is_empty() {
            bail!("播放信息中没有可用视频源");
        }

        if let Some(source) = self
            .media_sources
            .iter()
            .find(|source| source.id.as_deref() == Some(media_source_id))
        {
            source.direct_stream_url()?;
            return Ok(source);
        }

        if self.media_sources.len() == 1 {
            self.media_sources[0].direct_stream_url()?;
            return Ok(&self.media_sources[0]);
        }

        bail!("播放信息中未找到所选视频源");
    }

    pub fn direct_stream_url_for(&self, media_source_id: &str) -> Result<&str> {
        self.direct_stream_source_for(media_source_id)?
            .direct_stream_url()
    }
}

impl PlaybackMediaSource {
    pub fn direct_stream_url(&self) -> Result<&str> {
        self.direct_stream_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| anyhow!("播放信息缺少直链播放地址"))
    }
}

pub fn resolve_direct_stream_url(server: &CachedServer, direct_stream_url: &str) -> Result<Url> {
    let direct_stream_url = direct_stream_url.trim();
    if direct_stream_url.is_empty() {
        bail!("播放直链地址为空");
    }
    if let Ok(url) = Url::parse(direct_stream_url) {
        return Ok(url);
    }

    let base = server.endpoint.base_url()?;
    let relative = direct_stream_relative_path(&base, direct_stream_url);
    base.join(&relative).context("拼接播放直链地址失败")
}

fn direct_stream_relative_path(base: &Url, direct_stream_url: &str) -> String {
    if !direct_stream_url.starts_with('/') {
        return direct_stream_url.to_string();
    }

    let trimmed = direct_stream_url.trim_start_matches('/');
    let base_path = base.path().trim_matches('/');
    if !base_path.is_empty()
        && (trimmed == base_path
            || trimmed.starts_with(&format!("{base_path}/"))
            || base_path.ends_with("emby") && trimmed.starts_with("emby/"))
    {
        return format!("/{trimmed}");
    }

    trimmed.to_string()
}

fn authenticated_user_id(server: &CachedServer) -> Result<&str> {
    server
        .user_id
        .as_deref()
        .filter(|user_id| !user_id.trim().is_empty())
        .ok_or_else(|| anyhow!("Emby 用户 ID 缺失，请重新登录服务器"))
}

fn validate_item_id(item_id: &str) -> Result<()> {
    if item_id.trim().is_empty() {
        bail!("Emby 项目 ID 不能为空");
    }

    Ok(())
}

fn validate_media_source_id(media_source_id: &str) -> Result<()> {
    if media_source_id.trim().is_empty() {
        bail!("Emby 视频源 ID 不能为空");
    }

    Ok(())
}

fn add_playback_info_query(url: &mut Url, media_source_id: &str, user_id: &str) {
    url.query_pairs_mut()
        .append_pair("AutoOpenLiveStream", "false")
        .append_pair("IsPlayback", "false")
        .append_pair("MediaSourceId", media_source_id)
        .append_pair("UserId", user_id);
}

#[cfg(test)]
mod tests {
    use crate::server::{CachedServer, Protocol, ServerEndpoint};

    use super::*;

    fn endpoint() -> ServerEndpoint {
        ServerEndpoint {
            protocol: Protocol::Https,
            address: "example.com".to_string(),
            port: 443,
            path: "/emby".to_string(),
        }
    }

    fn nested_endpoint() -> ServerEndpoint {
        ServerEndpoint {
            protocol: Protocol::Https,
            address: "example.com".to_string(),
            port: 443,
            path: "/custom".to_string(),
        }
    }

    fn server_with_endpoint(endpoint: ServerEndpoint) -> CachedServer {
        CachedServer {
            id: "server-playback-test".to_string(),
            endpoint,
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

    fn server() -> CachedServer {
        server_with_endpoint(endpoint())
    }

    #[test]
    fn builds_playback_info_url() {
        let mut url =
            crate::emby::api_url(&endpoint(), &["Items", "795341", "PlaybackInfo"]).unwrap();
        add_playback_info_query(&mut url, "mediasource_795341", "user-1");

        assert_eq!(
            url.as_str(),
            "https://example.com/emby/Items/795341/PlaybackInfo?AutoOpenLiveStream=false&IsPlayback=false&MediaSourceId=mediasource_795341&UserId=user-1"
        );
    }

    #[test]
    fn parses_playback_info_direct_stream_url() {
        let json = r#"
        {
            "MediaSources": [
                {
                    "Id": "mediasource_795341",
                    "Size": 2034264644,
                    "DirectStreamUrl": "/videos/795341/original.mp4?MediaSourceId=mediasource_795341"
                }
            ],
            "PlaySessionId": "play-session-1"
        }
        "#;

        let playback_info: PlaybackInfo = serde_json::from_str(json).unwrap();

        assert_eq!(
            playback_info.play_session_id.as_deref(),
            Some("play-session-1")
        );
        assert_eq!(
            playback_info
                .direct_stream_url_for("mediasource_795341")
                .unwrap(),
            "/videos/795341/original.mp4?MediaSourceId=mediasource_795341"
        );
    }

    #[test]
    fn rejects_missing_direct_stream_url() {
        let playback_info = PlaybackInfo {
            media_sources: vec![PlaybackMediaSource {
                id: Some("source-1".to_string()),
                direct_stream_url: None,
            }],
            play_session_id: None,
        };

        assert!(playback_info.direct_stream_url_for("source-1").is_err());
    }

    #[test]
    fn rejects_empty_ids_before_building_requests() {
        let client = EmbyClient::new("device-1".to_string()).unwrap();
        let server = server();

        assert!(client.playback_info(&server, " ", "source-1").is_err());
        assert!(client.playback_info(&server, "episode-1", " ").is_err());
    }

    #[test]
    fn resolves_absolute_direct_stream_url() {
        let resolved = resolve_direct_stream_url(
            &server(),
            "https://cdn.example.com/videos/1/original.mp4?api_key=token",
        )
        .unwrap();

        assert_eq!(
            resolved.as_str(),
            "https://cdn.example.com/videos/1/original.mp4?api_key=token"
        );
    }

    #[test]
    fn resolves_emby_rooted_direct_stream_url() {
        let resolved = resolve_direct_stream_url(&server(), "/emby/videos/1/original.mp4").unwrap();

        assert_eq!(
            resolved.as_str(),
            "https://example.com/emby/videos/1/original.mp4"
        );
    }

    #[test]
    fn resolves_rooted_videos_direct_stream_url_under_base_url() {
        let resolved = resolve_direct_stream_url(&server(), "/videos/1/original.mp4").unwrap();

        assert_eq!(
            resolved.as_str(),
            "https://example.com/emby/videos/1/original.mp4"
        );
    }

    #[test]
    fn resolves_relative_direct_stream_url_under_base_url() {
        let resolved = resolve_direct_stream_url(&server(), "videos/1/original.mp4").unwrap();

        assert_eq!(
            resolved.as_str(),
            "https://example.com/emby/videos/1/original.mp4"
        );
    }

    #[test]
    fn preserves_nested_base_for_rooted_videos_direct_stream_url() {
        let resolved = resolve_direct_stream_url(
            &server_with_endpoint(nested_endpoint()),
            "/videos/1/original.mp4",
        )
        .unwrap();

        assert_eq!(
            resolved.as_str(),
            "https://example.com/custom/emby/videos/1/original.mp4"
        );
    }
}
