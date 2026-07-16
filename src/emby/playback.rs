use anyhow::{Context, Result, anyhow, bail};
use reqwest::Method;
use serde::{Deserialize, Serialize};
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

    #[instrument(skip(self, server, report), fields(server = %server.endpoint.display_url(), item_id = %report.item_id, media_source_id = %report.media_source_id))]
    pub fn report_playback_started(
        &self,
        server: &CachedServer,
        report: &PlaybackStartReport,
    ) -> Result<()> {
        validate_report_ids(&report.item_id, &report.media_source_id)?;
        let url = api_url(&server.endpoint, &["Sessions", "Playing"])?;
        self.send_authenticated_body_url(server, Method::POST, url, report)
            .map(|_| ())
            .context("上报 Emby 播放开始失败")
    }

    #[instrument(skip(self, server, report), fields(server = %server.endpoint.display_url(), item_id = %report.item_id, media_source_id = %report.media_source_id))]
    pub fn report_playback_progress(
        &self,
        server: &CachedServer,
        report: &PlaybackProgressReport,
    ) -> Result<()> {
        validate_report_ids(&report.item_id, &report.media_source_id)?;
        let url = api_url(&server.endpoint, &["Sessions", "Playing", "Progress"])?;
        self.send_authenticated_body_url(server, Method::POST, url, report)
            .map(|_| ())
            .context("上报 Emby 播放进度失败")
    }

    #[instrument(skip(self, server, report), fields(server = %server.endpoint.display_url(), item_id = %report.item_id, media_source_id = %report.media_source_id))]
    pub fn report_playback_stopped(
        &self,
        server: &CachedServer,
        report: &PlaybackStopReport,
    ) -> Result<()> {
        validate_report_ids(&report.item_id, &report.media_source_id)?;
        let url = api_url(&server.endpoint, &["Sessions", "Playing", "Stopped"])?;
        self.send_authenticated_body_url(server, Method::POST, url, report)
            .map(|_| ())
            .context("上报 Emby 播放停止失败")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackQueueReportItem {
    pub id: String,
    pub playlist_item_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackStartReport {
    pub can_seek: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_stream_index: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_stream_index: Option<i32>,
    pub is_paused: bool,
    pub is_muted: bool,
    pub position_ticks: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_time_ticks: Option<u64>,
    pub volume_level: i32,
    pub play_method: PlaybackReportPlayMethod,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_session_id: Option<String>,
    pub repeat_mode: PlaybackReportRepeatMode,
    pub playback_rate: f64,
    pub item_id: String,
    pub media_source_id: String,
    pub playlist_length: usize,
    pub playlist_index: i32,
    pub playlist_item_id: String,
    pub now_playing_queue: Vec<PlaybackQueueReportItem>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackProgressReport {
    pub can_seek: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_stream_index: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_stream_index: Option<i32>,
    pub is_paused: bool,
    pub is_muted: bool,
    pub position_ticks: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_time_ticks: Option<u64>,
    pub volume_level: i32,
    pub play_method: PlaybackReportPlayMethod,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_session_id: Option<String>,
    pub repeat_mode: PlaybackReportRepeatMode,
    pub playback_rate: f64,
    pub event_name: PlaybackProgressEventName,
    pub item_id: String,
    pub media_source_id: String,
    pub playlist_length: usize,
    pub playlist_index: i32,
    pub playlist_item_id: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackStopReport {
    pub can_seek: bool,
    pub is_paused: bool,
    pub playback_rate: f64,
    pub failed: bool,
    pub position_ticks: u64,
    pub item_id: String,
    pub media_source_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_session_id: Option<String>,
    pub playlist_length: usize,
    pub playlist_index: i32,
    pub now_playing_queue: Vec<PlaybackQueueReportItem>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum PlaybackReportPlayMethod {
    DirectStream,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum PlaybackReportRepeatMode {
    RepeatNone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum PlaybackProgressEventName {
    #[serde(rename = "timeupdate")]
    TimeUpdate,
}

impl PlaybackStartReport {
    pub fn direct_stream(
        item_id: String,
        media_source_id: String,
        playlist_item_id: String,
        now_playing_queue: Vec<PlaybackQueueReportItem>,
    ) -> Self {
        Self {
            can_seek: false,
            audio_stream_index: None,
            subtitle_stream_index: None,
            is_paused: false,
            is_muted: false,
            position_ticks: 0,
            run_time_ticks: None,
            volume_level: 100,
            play_method: PlaybackReportPlayMethod::DirectStream,
            play_session_id: None,
            repeat_mode: PlaybackReportRepeatMode::RepeatNone,
            playback_rate: 1.0,
            item_id,
            media_source_id,
            playlist_length: now_playing_queue.len(),
            playlist_index: 0,
            playlist_item_id,
            now_playing_queue,
        }
    }
}

impl PlaybackProgressReport {
    pub fn direct_stream(
        item_id: String,
        media_source_id: String,
        playlist_item_id: String,
        playlist_length: usize,
        playlist_index: i32,
    ) -> Self {
        Self {
            can_seek: false,
            audio_stream_index: None,
            subtitle_stream_index: None,
            is_paused: false,
            is_muted: false,
            position_ticks: 0,
            run_time_ticks: None,
            volume_level: 100,
            play_method: PlaybackReportPlayMethod::DirectStream,
            play_session_id: None,
            repeat_mode: PlaybackReportRepeatMode::RepeatNone,
            playback_rate: 1.0,
            event_name: PlaybackProgressEventName::TimeUpdate,
            item_id,
            media_source_id,
            playlist_length,
            playlist_index,
            playlist_item_id,
        }
    }
}

impl PlaybackStopReport {
    pub fn direct_stream(item_id: String, media_source_id: String) -> Self {
        Self {
            can_seek: false,
            is_paused: false,
            playback_rate: 1.0,
            failed: false,
            position_ticks: 0,
            item_id,
            media_source_id,
            play_session_id: None,
            playlist_length: 0,
            playlist_index: -1,
            now_playing_queue: Vec::new(),
        }
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
    #[serde(alias = "ContentLength")]
    pub size: Option<u64>,
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

fn validate_report_ids(item_id: &str, media_source_id: &str) -> Result<()> {
    validate_item_id(item_id)?;
    validate_media_source_id(media_source_id)
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
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

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
    fn playback_report_urls_match_emby_session_endpoints() {
        assert_eq!(
            api_url(&endpoint(), &["Sessions", "Playing"])
                .unwrap()
                .as_str(),
            "https://example.com/emby/Sessions/Playing"
        );
        assert_eq!(
            api_url(&endpoint(), &["Sessions", "Playing", "Progress"])
                .unwrap()
                .as_str(),
            "https://example.com/emby/Sessions/Playing/Progress"
        );
        assert_eq!(
            api_url(&endpoint(), &["Sessions", "Playing", "Stopped"])
                .unwrap()
                .as_str(),
            "https://example.com/emby/Sessions/Playing/Stopped"
        );
    }

    #[test]
    fn playback_report_accepts_empty_success_response() {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("failed to bind playback report test server: {error}"),
        };
        let port = listener.local_addr().unwrap().port();
        let server_thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            request
        });
        let server = server_with_endpoint(ServerEndpoint {
            protocol: Protocol::Http,
            address: "127.0.0.1".to_string(),
            port,
            path: String::new(),
        });
        let client = EmbyClient::new("device-1".to_string()).unwrap();
        let report = PlaybackStartReport::direct_stream(
            "episode-1".to_string(),
            "source-1".to_string(),
            "playlistItem0".to_string(),
            vec![PlaybackQueueReportItem {
                id: "episode-1".to_string(),
                playlist_item_id: "playlistItem0".to_string(),
            }],
        );

        client.report_playback_started(&server, &report).unwrap();

        let request = server_thread.join().unwrap();
        assert!(request.starts_with("POST /emby/Sessions/Playing HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("content-type: application/json")
        );
        assert!(request.contains("\"ItemId\":\"episode-1\""));
    }

    #[test]
    fn serializes_playback_started_with_string_queue_ids() {
        let queue = vec![
            PlaybackQueueReportItem {
                id: "episode-1".to_string(),
                playlist_item_id: "playlistItem0".to_string(),
            },
            PlaybackQueueReportItem {
                id: "episode-2".to_string(),
                playlist_item_id: "playlistItem1".to_string(),
            },
        ];
        let mut report = PlaybackStartReport::direct_stream(
            "episode-2".to_string(),
            "source-2".to_string(),
            "playlistItem1".to_string(),
            queue,
        );
        report.can_seek = true;
        report.audio_stream_index = Some(1);
        report.subtitle_stream_index = Some(4);
        report.position_ticks = 199_840_000;
        report.run_time_ticks = Some(17_341_860_000);
        report.play_session_id = Some("session-2".to_string());
        report.playlist_index = 1;

        let value = serde_json::to_value(&report).unwrap();

        assert_eq!(value["PlayMethod"], "DirectStream");
        assert_eq!(value["RepeatMode"], "RepeatNone");
        assert_eq!(value["PlaylistLength"], 2);
        assert_eq!(value["PlaylistIndex"], 1);
        assert_eq!(value["NowPlayingQueue"][0]["Id"], "episode-1");
        assert_eq!(
            value["NowPlayingQueue"][1]["PlaylistItemId"],
            "playlistItem1"
        );
        assert_eq!(value["PositionTicks"], 199_840_000_u64);
        assert_eq!(value["PlaySessionId"], "session-2");
    }

    #[test]
    fn serializes_progress_timeupdate_without_now_playing_queue() {
        let report = PlaybackProgressReport::direct_stream(
            "episode-2".to_string(),
            "source-2".to_string(),
            "playlistItem1".to_string(),
            3,
            1,
        );

        let value = serde_json::to_value(&report).unwrap();

        assert_eq!(value["EventName"], "timeupdate");
        assert_eq!(value["PlaylistLength"], 3);
        assert_eq!(value["PlaylistIndex"], 1);
        assert!(value.get("NowPlayingQueue").is_none());
        assert!(value.get("PlaySessionId").is_none());
        assert!(value.get("AudioStreamIndex").is_none());
        assert!(value.get("SubtitleStreamIndex").is_none());
    }

    #[test]
    fn serializes_stopped_with_terminal_playlist_state() {
        let mut report =
            PlaybackStopReport::direct_stream("episode-2".to_string(), "source-2".to_string());
        report.position_ticks = 1_081_540_000;
        report.failed = true;

        let value = serde_json::to_value(&report).unwrap();

        assert_eq!(value["PlaylistLength"], 0);
        assert_eq!(value["PlaylistIndex"], -1);
        assert_eq!(value["NowPlayingQueue"], serde_json::json!([]));
        assert_eq!(value["Failed"], true);
        assert_eq!(value["PositionTicks"], 1_081_540_000_u64);
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
        assert_eq!(
            playback_info
                .direct_stream_source_for("mediasource_795341")
                .unwrap()
                .size,
            Some(2034264644)
        );
    }

    #[test]
    fn parses_playback_info_content_length_alias() {
        let json = r#"
        {
            "MediaSources": [
                {
                    "Id": "mediasource_795341",
                    "ContentLength": 2034264644,
                    "DirectStreamUrl": "/videos/795341/original.mp4?MediaSourceId=mediasource_795341"
                }
            ]
        }
        "#;

        let playback_info: PlaybackInfo = serde_json::from_str(json).unwrap();

        assert_eq!(
            playback_info
                .direct_stream_source_for("mediasource_795341")
                .unwrap()
                .size,
            Some(2034264644)
        );
    }

    #[test]
    fn rejects_missing_direct_stream_url() {
        let playback_info = PlaybackInfo {
            media_sources: vec![PlaybackMediaSource {
                id: Some("source-1".to_string()),
                size: None,
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

    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        let mut expected_len = None;
        loop {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            if expected_len.is_none()
                && let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                expected_len = Some(header_end + 4 + content_length);
            }
            if expected_len.is_some_and(|expected| bytes.len() >= expected) {
                break;
            }
        }
        String::from_utf8(bytes).unwrap()
    }
}
