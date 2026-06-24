use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use reqwest::Method;
use serde::Deserialize;
use tracing::instrument;

use crate::server::CachedServer;

use super::{EmbyClient, api_url};

const SHOW_SEASONS_FIELDS: &str = "BasicSyncInfo,CommunityRating,ProductionYear,EndDate,Container";
const SHOW_NEXT_UP_LIMIT: u32 = 1;
const SHOW_EPISODES_ENABLE_IMAGE_TYPES: &str = "Primary,Backdrop,Thumb";
const SHOW_EPISODES_FIELDS: &str = "BasicSyncInfo,RunTimeTicks,CommunityRating,ProviderIds,ProductionYear,EndDate,Container,Overview,UserData,MediaSources,People,CanDownload,DateCreated,MediaStreams,Path,ParentId,Studios,AlternateMediaSources";

impl EmbyClient {
    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url(), item_id = %item_id))]
    pub fn media_item(&self, server: &CachedServer, item_id: &str) -> Result<MediaItem> {
        validate_item_id(item_id)?;
        let user_id = authenticated_user_id(server)?;
        self.send_authenticated_json(
            server,
            Method::GET,
            &["Users", user_id, "Items", item_id],
            "解析 Emby 媒体详情响应失败",
        )
    }

    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url(), series_id = %series_id))]
    pub fn show_seasons(&self, server: &CachedServer, series_id: &str) -> Result<MediaItems> {
        validate_item_id(series_id)?;
        let user_id = authenticated_user_id(server)?;
        let mut url = api_url(&server.endpoint, &["Shows", series_id, "Seasons"])?;
        add_show_seasons_query(&mut url, user_id);
        self.send_authenticated_json_url(server, Method::GET, url, "解析 Emby 剧集季数响应失败")
    }

    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url(), series_id = %series_id))]
    pub fn show_next_up(&self, server: &CachedServer, series_id: &str) -> Result<MediaItems> {
        validate_item_id(series_id)?;
        let user_id = authenticated_user_id(server)?;
        let mut url = api_url(&server.endpoint, &["Shows", "NextUp"])?;
        add_show_next_up_query(&mut url, series_id, user_id);
        self.send_authenticated_json_url(server, Method::GET, url, "解析 Emby 下一剧集响应失败")
    }

    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url(), series_id = %series_id, season_id = ?season_id))]
    pub fn show_episodes(
        &self,
        server: &CachedServer,
        series_id: &str,
        season_id: Option<&str>,
    ) -> Result<MediaItems> {
        validate_item_id(series_id)?;
        if let Some(season_id) = season_id {
            validate_item_id(season_id)?;
        }
        let user_id = authenticated_user_id(server)?;
        let mut url = api_url(&server.endpoint, &["Shows", series_id, "Episodes"])?;
        add_show_episodes_query(&mut url, season_id, user_id);
        self.send_authenticated_json_url(server, Method::GET, url, "解析 Emby 剧集分集响应失败")
    }
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

fn add_show_seasons_query(url: &mut url::Url, user_id: &str) {
    url.query_pairs_mut()
        .append_pair("Fields", SHOW_SEASONS_FIELDS)
        .append_pair("UserId", user_id);
}

fn add_show_next_up_query(url: &mut url::Url, series_id: &str, user_id: &str) {
    url.query_pairs_mut()
        .append_pair("Limit", &SHOW_NEXT_UP_LIMIT.to_string())
        .append_pair("SeriesId", series_id)
        .append_pair("UserId", user_id);
}

fn add_show_episodes_query(url: &mut url::Url, season_id: Option<&str>, user_id: &str) {
    let mut query = url.query_pairs_mut();
    query
        .append_pair("EnableImageTypes", SHOW_EPISODES_ENABLE_IMAGE_TYPES)
        .append_pair("Fields", SHOW_EPISODES_FIELDS);
    if let Some(season_id) = season_id {
        query.append_pair("SeasonId", season_id);
    }
    query.append_pair("UserId", user_id);
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaItems {
    #[serde(default)]
    pub items: Vec<MediaItem>,
    #[serde(default)]
    pub total_record_count: u32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaItem {
    pub id: String,
    pub name: String,
    #[serde(rename = "Type")]
    pub item_type: Option<String>,
    pub server_id: Option<String>,
    pub production_year: Option<u32>,
    pub premiere_date: Option<String>,
    pub run_time_ticks: Option<u64>,
    pub index_number: Option<u32>,
    pub parent_index_number: Option<u32>,
    pub is_folder: Option<bool>,
    pub community_rating: Option<f32>,
    pub official_rating: Option<String>,
    pub genres: Option<Vec<String>>,
    pub overview: Option<String>,
    pub series_name: Option<String>,
    pub series_id: Option<String>,
    pub season_id: Option<String>,
    pub season_name: Option<String>,
    pub media_type: Option<String>,
    pub image_tags: Option<HashMap<String, String>>,
    pub backdrop_image_tags: Option<Vec<String>>,
    pub parent_logo_item_id: Option<String>,
    pub parent_logo_image_tag: Option<String>,
    pub parent_backdrop_item_id: Option<String>,
    pub parent_backdrop_image_tags: Option<Vec<String>>,
    pub series_primary_image_tag: Option<String>,
    pub media_sources: Option<Vec<MediaSource>>,
    pub people: Option<Vec<MediaPerson>>,
}

impl MediaItem {
    pub fn image_tag(&self, image_type: &str) -> Option<&str> {
        self.image_tags
            .as_ref()
            .and_then(|tags| tags.get(image_type))
            .map(String::as_str)
            .filter(|tag| !tag.trim().is_empty())
    }

    pub fn primary_image_tag(&self) -> Option<&str> {
        self.image_tag("Primary")
    }

    pub fn logo_image_tag(&self) -> Option<&str> {
        self.image_tag("Logo")
    }

    pub fn backdrop_image_tag(&self) -> Option<&str> {
        first_non_empty_tag(self.backdrop_image_tags.as_deref())
    }

    pub fn episode_label(&self) -> String {
        match (self.parent_index_number, self.index_number) {
            (Some(season), Some(episode)) => format!("S{season}E{episode}: {}", self.name),
            (_, Some(episode)) => format!("E{episode}: {}", self.name),
            _ => self.name.clone(),
        }
    }

    pub fn episode_card_label(&self) -> String {
        match self.index_number {
            Some(episode) => format!("E{episode}: {}", self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaPerson {
    pub name: Option<String>,
    pub id: Option<String>,
    pub role: Option<String>,
    #[serde(rename = "Type")]
    pub person_type: Option<String>,
    pub primary_image_tag: Option<String>,
}

impl MediaPerson {
    pub fn display_name(&self) -> String {
        non_empty_string(self.name.as_deref()).unwrap_or_else(|| "未知人员".to_string())
    }

    pub fn role_label(&self) -> String {
        non_empty_string(self.role.as_deref()).unwrap_or_else(|| "暂无角色".to_string())
    }

    pub fn type_label(&self) -> String {
        non_empty_string(self.person_type.as_deref()).unwrap_or_else(|| "未知类型".to_string())
    }

    pub fn id(&self) -> Option<&str> {
        non_empty_str(self.id.as_deref())
    }

    pub fn primary_image_tag(&self) -> Option<&str> {
        non_empty_str(self.primary_image_tag.as_deref())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaSource {
    pub id: Option<String>,
    pub name: Option<String>,
    pub path: Option<String>,
    pub container: Option<String>,
    pub media_streams: Option<Vec<MediaStream>>,
}

impl MediaSource {
    pub fn display_name(&self, index: usize) -> String {
        self.name
            .as_deref()
            .or(self.container.as_deref())
            .filter(|name| !name.trim().is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("视频 {}", index + 1))
    }

    pub fn name_label(&self, index: usize) -> String {
        self.name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("视频 {}", index + 1))
    }

    pub fn subtitle_streams(&self) -> Vec<&MediaStream> {
        self.media_streams
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|stream| stream.is_subtitle())
            .collect()
    }

    pub fn audio_streams(&self) -> Vec<&MediaStream> {
        self.media_streams
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|stream| stream.is_audio())
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct MediaStream {
    pub index: Option<u32>,
    #[serde(rename = "Type")]
    pub stream_type: Option<String>,
    pub display_title: Option<String>,
    pub title: Option<String>,
    pub language: Option<String>,
    pub codec: Option<String>,
    pub delivery_url: Option<String>,
    pub delivery_method: Option<String>,
    pub is_external: Option<bool>,
    pub is_default: Option<bool>,
    pub is_text_subtitle_stream: Option<bool>,
    pub supports_external_stream: Option<bool>,
}

impl MediaStream {
    pub fn is_subtitle(&self) -> bool {
        self.stream_type
            .as_deref()
            .is_some_and(|stream_type| stream_type.eq_ignore_ascii_case("Subtitle"))
    }

    pub fn is_audio(&self) -> bool {
        self.stream_type
            .as_deref()
            .is_some_and(|stream_type| stream_type.eq_ignore_ascii_case("Audio"))
    }

    pub fn display_label(&self, index: usize) -> String {
        self.display_title
            .as_deref()
            .or(self.title.as_deref())
            .or(self.language.as_deref())
            .filter(|title| !title.trim().is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("字幕 {}", index + 1))
    }

    pub fn display_title_label(&self, index: usize) -> String {
        self.display_title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("字幕 {}", index + 1))
    }

    pub fn audio_label(&self, index: usize) -> String {
        self.display_title
            .as_deref()
            .or(self.title.as_deref())
            .or(self.language.as_deref())
            .or(self.codec.as_deref())
            .filter(|title| !title.trim().is_empty())
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("音轨 {}", index + 1))
    }
}

fn first_non_empty_tag(tags: Option<&[String]>) -> Option<&str> {
    tags.and_then(|tags| {
        tags.iter()
            .map(String::as_str)
            .find(|tag| !tag.trim().is_empty())
    })
}

fn non_empty_str(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn non_empty_string(value: Option<&str>) -> Option<String> {
    non_empty_str(value).map(ToString::to_string)
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

    fn server() -> CachedServer {
        CachedServer {
            id: "server-show-test".to_string(),
            endpoint: endpoint(),
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
    fn builds_media_item_detail_url() {
        let url =
            crate::emby::api_url(&endpoint(), &["Users", "user-1", "Items", "771013"]).unwrap();

        assert_eq!(
            url.as_str(),
            "https://example.com/emby/Users/user-1/Items/771013"
        );
    }

    #[test]
    fn builds_show_seasons_url() {
        let mut url = crate::emby::api_url(&endpoint(), &["Shows", "771013", "Seasons"]).unwrap();
        add_show_seasons_query(&mut url, "user-1");

        assert_eq!(
            url.as_str(),
            "https://example.com/emby/Shows/771013/Seasons?Fields=BasicSyncInfo%2CCommunityRating%2CProductionYear%2CEndDate%2CContainer&UserId=user-1"
        );
    }

    #[test]
    fn builds_show_next_up_url() {
        let mut url = crate::emby::api_url(&endpoint(), &["Shows", "NextUp"]).unwrap();
        add_show_next_up_query(&mut url, "771013", "user-1");

        assert_eq!(
            url.as_str(),
            "https://example.com/emby/Shows/NextUp?Limit=1&SeriesId=771013&UserId=user-1"
        );
    }

    #[test]
    fn builds_show_episodes_url() {
        let mut url = crate::emby::api_url(&endpoint(), &["Shows", "771013", "Episodes"]).unwrap();
        add_show_episodes_query(&mut url, Some("776641"), "user-1");

        assert_eq!(
            url.as_str(),
            "https://example.com/emby/Shows/771013/Episodes?EnableImageTypes=Primary%2CBackdrop%2CThumb&Fields=BasicSyncInfo%2CRunTimeTicks%2CCommunityRating%2CProviderIds%2CProductionYear%2CEndDate%2CContainer%2COverview%2CUserData%2CMediaSources%2CPeople%2CCanDownload%2CDateCreated%2CMediaStreams%2CPath%2CParentId%2CStudios%2CAlternateMediaSources&SeasonId=776641&UserId=user-1"
        );
    }

    #[test]
    fn parses_series_detail_fields() {
        let json = r#"
        {
            "Name": "剑来",
            "Id": "771013",
            "Type": "Series",
            "CommunityRating": 8.5,
            "OfficialRating": "TV-14",
            "Genres": ["动画", "动作"],
            "ImageTags": {
                "Primary": "primary-tag",
                "Logo": "logo-tag"
            },
            "BackdropImageTags": ["backdrop-tag"],
            "People": [
                {
                    "Name": "张三",
                    "Id": "56058",
                    "Role": "陈平安",
                    "Type": "Actor",
                    "PrimaryImageTag": "person-primary"
                },
                {
                    "Name": "李四",
                    "Id": "56059",
                    "Type": "Director",
                    "PrimaryImageTag": null
                }
            ]
        }
        "#;

        let item: MediaItem = serde_json::from_str(json).unwrap();
        let people = item.people.as_deref().unwrap();

        assert_eq!(item.item_type.as_deref(), Some("Series"));
        assert_eq!(item.logo_image_tag(), Some("logo-tag"));
        assert_eq!(item.backdrop_image_tag(), Some("backdrop-tag"));
        assert_eq!(item.genres.as_deref().unwrap(), ["动画", "动作"]);
        assert_eq!(people[0].display_name(), "张三");
        assert_eq!(people[0].role_label(), "陈平安");
        assert_eq!(people[0].type_label(), "Actor");
        assert_eq!(people[0].id(), Some("56058"));
        assert_eq!(people[0].primary_image_tag(), Some("person-primary"));
        assert_eq!(people[1].role_label(), "暂无角色");
        assert_eq!(people[1].primary_image_tag(), None);
    }

    #[test]
    fn parses_seasons_and_episode_media_sources() {
        let json = r#"
        {
            "Items": [
                {
                    "Name": "止境",
                    "Id": "1085642",
                    "IndexNumber": 5,
                    "ParentIndexNumber": 1,
                    "Type": "Episode",
                    "SeasonId": "776641",
                    "ImageTags": {
                        "Primary": "episode-primary"
                    },
                    "MediaSources": [
                        {
                            "Id": "source-1",
                            "Name": "1080p - 8 Mbps",
                            "MediaStreams": [
                                { "Index": 0, "Type": "Video", "DisplayTitle": "1080p HEVC" },
                                { "Index": 1, "Type": "Subtitle", "DisplayTitle": "简体中文" }
                            ]
                        }
                    ]
                }
            ],
            "TotalRecordCount": 1
        }
        "#;

        let items: MediaItems = serde_json::from_str(json).unwrap();
        let episode = &items.items[0];
        let source = episode.media_sources.as_ref().unwrap().first().unwrap();
        let subtitles = source.subtitle_streams();

        assert_eq!(items.total_record_count, 1);
        assert_eq!(episode.episode_label(), "S1E5: 止境");
        assert_eq!(episode.primary_image_tag(), Some("episode-primary"));
        assert_eq!(source.display_name(0), "1080p - 8 Mbps");
        assert_eq!(source.name_label(0), "1080p - 8 Mbps");
        assert_eq!(subtitles[0].display_label(0), "简体中文");
        assert_eq!(subtitles[0].display_title_label(0), "简体中文");
    }

    #[test]
    fn rejects_empty_ids_before_building_requests() {
        let client = EmbyClient::new("device-1".to_string()).unwrap();
        let server = server();

        assert!(client.media_item(&server, " ").is_err());
        assert!(client.show_seasons(&server, " ").is_err());
        assert!(client.show_next_up(&server, " ").is_err());
        assert!(
            client
                .show_episodes(&server, "series-1", Some(" "))
                .is_err()
        );
    }
}
