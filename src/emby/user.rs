use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument};

use crate::server::{AddServerSubmission, CachedServer};

use super::{CLIENT_NAME, EmbyClient, EmbyImageType, VERSION, api_url, log_secrets};

impl EmbyClient {
    #[instrument(skip(self, submission), fields(server = %submission.endpoint.display_url(), username = %submission.username))]
    pub fn authenticate_by_name(&self, submission: &AddServerSubmission) -> Result<AuthSession> {
        let mut url = submission.endpoint.base_url()?;
        url.path_segments_mut()
            .map_err(|_| anyhow!("服务器地址不能作为 API 基础地址"))?
            .pop_if_empty()
            .extend(["Users", "AuthenticateByName"]);

        let authorization = self.authorization_header();
        let request_body = AuthenticateUserByName {
            username: &submission.username,
            password: &submission.password,
            pw: &submission.password,
        };
        let request_body_json =
            serde_json::to_string(&request_body).context("序列化 Emby 认证请求失败")?;

        debug!(method = "POST", url = %url, "sending Emby authentication request");
        if log_secrets() {
            debug!(
                method = "POST",
                url = %url,
                x_emby_authorization = %authorization,
                content_type = "application/json",
                body = %request_body_json,
                "full Emby authentication request"
            );
        }

        let response = self
            .http
            .post(url)
            .header("X-Emby-Authorization", authorization)
            .header("Content-Type", "application/json")
            .header("User-Agent", format!("{CLIENT_NAME}/{VERSION}"))
            .body(request_body_json)
            .send()
            .context("连接 Emby 服务器失败")?;

        let status = response.status();
        let response_headers = format!("{:?}", response.headers());
        let response_body = response.text().context("读取 Emby 认证响应失败")?;
        debug!(status = %status, "received Emby authentication response");
        if log_secrets() {
            debug!(
                status = %status,
                headers = %response_headers,
                body = %response_body,
                "full Emby authentication response"
            );
        }

        if !status.is_success() {
            bail!("Emby 认证失败：HTTP {status} {response_body}");
        }

        let session = serde_json::from_str::<AuthSession>(&response_body)
            .context("解析 Emby 认证响应失败")?;
        if log_secrets() {
            debug!(access_token = %session.access_token, "received Emby access token");
        }

        Ok(session)
    }

    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url()))]
    pub fn user_views(&self, server: &CachedServer) -> Result<UserViews> {
        let user_id = authenticated_user_id(server)?;
        let mut url = api_url(&server.endpoint, &["Users", user_id, "Views"])?;
        url.query_pairs_mut()
            .append_pair("IncludeExternalContent", "false");
        self.send_authenticated_json_url(server, Method::GET, url, "解析 Emby 用户视图响应失败")
    }

    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url()))]
    pub fn resume_items(&self, server: &CachedServer) -> Result<ResumeItems> {
        let user_id = authenticated_user_id(server)?;
        let mut url = api_url(&server.endpoint, &["Users", user_id, "Items", "Resume"])?;
        add_resume_items_query(&mut url);
        self.send_authenticated_json_url(server, Method::GET, url, "解析 Emby 继续观看响应失败")
    }

    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url(), parent_id = %parent_id))]
    pub fn user_items(
        &self,
        server: &CachedServer,
        parent_id: &str,
        start_index: u32,
        limit: u32,
        sort_order: SortOrder,
    ) -> Result<UserItems> {
        self.query_user_items(
            server,
            &UserItemsQuery {
                parent_id: Some(parent_id.to_string()),
                include_item_types: vec![VideoItemType::Movie, VideoItemType::Series],
                start_index,
                limit,
                sort_by: Some(UserItemsSort::DateLastContentAdded),
                sort_order,
                ..UserItemsQuery::default()
            },
        )
    }

    #[instrument(skip(self, server, query), fields(server = %server.endpoint.display_url()))]
    pub fn query_user_items(
        &self,
        server: &CachedServer,
        query: &UserItemsQuery,
    ) -> Result<UserItems> {
        let user_id = authenticated_user_id(server)?;
        query.validate()?;
        let mut url = api_url(&server.endpoint, &["Users", user_id, "Items"])?;
        add_query_user_items_query(&mut url, query);
        self.send_authenticated_json_url(server, Method::GET, url, "解析 Emby 用户项目响应失败")
    }

    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url(), search_term = %search_term))]
    pub fn search_items(
        &self,
        server: &CachedServer,
        search_term: &str,
        start_index: u32,
        limit: u32,
    ) -> Result<UserItems> {
        let query = search_user_items_query(search_term, start_index, limit)?;
        self.query_user_items(server, &query)
    }

    #[instrument(skip(self, server, include_item_types), fields(server = %server.endpoint.display_url(), parent_id = %parent_id))]
    pub fn latest_items(
        &self,
        server: &CachedServer,
        parent_id: &str,
        include_item_types: &[VideoItemType],
        limit: u32,
    ) -> Result<Vec<UserItem>> {
        let user_id = authenticated_user_id(server)?;
        validate_non_empty_id(parent_id, "Emby 媒体库 ID")?;
        if limit == 0 {
            bail!("Emby 项目查询 Limit 必须大于 0");
        }
        let mut url = api_url(&server.endpoint, &["Users", user_id, "Items", "Latest"])?;
        add_latest_items_query(&mut url, parent_id, include_item_types, limit);
        self.send_authenticated_json_url(server, Method::GET, url, "解析 Emby 最新项目响应失败")
    }

    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url(), item_id = %item_id, favorite))]
    pub fn set_favorite(
        &self,
        server: &CachedServer,
        item_id: &str,
        favorite: bool,
    ) -> Result<UserItemData> {
        let user_id = authenticated_user_id(server)?;
        validate_non_empty_id(item_id, "Emby 项目 ID")?;
        let url = favorite_item_url(&server.endpoint, user_id, item_id)?;
        let method = favorite_method(favorite);
        self.send_authenticated_json_url(server, method, url, "解析 Emby 收藏状态响应失败")
    }

    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url(), item_id = %item_id))]
    pub fn mark_item_played(&self, server: &CachedServer, item_id: &str) -> Result<UserItemData> {
        let user_id = authenticated_user_id(server)?;
        validate_non_empty_id(item_id, "Emby 项目 ID")?;
        let url = played_item_url(&server.endpoint, user_id, item_id)?;
        self.send_authenticated_json_url(server, Method::POST, url, "解析 Emby 已观看状态响应失败")
    }

    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url(), item_id = %item_id))]
    pub fn hide_item_from_resume(&self, server: &CachedServer, item_id: &str) -> Result<()> {
        let user_id = authenticated_user_id(server)?;
        validate_non_empty_id(item_id, "Emby 项目 ID")?;
        let url = hide_from_resume_url(&server.endpoint, user_id, item_id)?;
        self.send_authenticated_url(server, Method::POST, url)?;
        Ok(())
    }
}

fn add_resume_items_query(url: &mut url::Url) {
    url.query_pairs_mut()
        .append_pair("EnableImages", "true")
        .append_pair("EnableImageTypes", "Primary,Backdrop,Thumb,Logo")
        .append_pair("EnableUserData", "true")
        .append_pair(
            "Fields",
            "BasicSyncInfo,Overview,Container,CanDelete,ProviderIds,ProductionYear,Genres,DateCreated,ParentId,SeriesId,SeriesName,IndexNumber,ParentIndexNumber,MediaType,CommunityRating,PrimaryImageAspectRatio,CollectionType,UserData,People,MediaSources,MediaStreams",
        )
        .append_pair("Limit", "30")
        .append_pair("MediaTypes", "Video")
        .append_pair("Recursive", "true");
}

const USER_ITEM_FIELDS: &str = "BasicSyncInfo,CollectionType,PrimaryImageAspectRatio,UserData,CommunityRating,ProviderIds,ProductionYear,ChildCount,Container,CanDelete,ParentId,SeriesId,SeriesName,IndexNumber,ParentIndexNumber,MediaType";
const SEARCH_USER_ITEM_FIELDS: &str =
    "BasicSyncInfo,CommunityRating,ProductionYear,EndDate,Container";

fn authenticated_user_id(server: &CachedServer) -> Result<&str> {
    server
        .user_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| anyhow!("Emby 用户 ID 缺失，请重新登录服务器"))
}

fn validate_non_empty_id(value: &str, label: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{label}不能为空");
    }
    Ok(())
}

fn search_user_items_query(
    search_term: &str,
    start_index: u32,
    limit: u32,
) -> Result<UserItemsQuery> {
    let query = UserItemsQuery {
        fields: Some(SEARCH_USER_ITEM_FIELDS.to_string()),
        include_item_types: vec![VideoItemType::Movie, VideoItemType::Series],
        group_programs_by_series: true,
        search_term: Some(search_term.trim().to_string()),
        recursive: true,
        start_index,
        limit,
        sort_by: Some(UserItemsSort::DateCreated),
        sort_order: SortOrder::Descending,
        ..UserItemsQuery::default()
    };
    query.validate()?;
    Ok(query)
}

fn favorite_item_url(
    endpoint: &crate::server::ServerEndpoint,
    user_id: &str,
    item_id: &str,
) -> Result<url::Url> {
    api_url(endpoint, &["Users", user_id, "FavoriteItems", item_id])
}

fn played_item_url(
    endpoint: &crate::server::ServerEndpoint,
    user_id: &str,
    item_id: &str,
) -> Result<url::Url> {
    api_url(endpoint, &["Users", user_id, "PlayedItems", item_id])
}

fn hide_from_resume_url(
    endpoint: &crate::server::ServerEndpoint,
    user_id: &str,
    item_id: &str,
) -> Result<url::Url> {
    let mut url = api_url(
        endpoint,
        &["Users", user_id, "Items", item_id, "HideFromResume"],
    )?;
    url.query_pairs_mut().append_pair("Hide", "true");
    Ok(url)
}

fn favorite_method(favorite: bool) -> Method {
    if favorite {
        Method::POST
    } else {
        Method::DELETE
    }
}

fn add_query_user_items_query(url: &mut url::Url, query: &UserItemsQuery) {
    let ids = stable_non_empty_ids(&query.ids);
    let include_types = item_types_query(&query.include_item_types);
    let fields = query
        .fields
        .as_deref()
        .map(str::trim)
        .filter(|fields| !fields.is_empty())
        .unwrap_or(USER_ITEM_FIELDS);
    let mut pairs = url.query_pairs_mut();
    pairs
        .append_pair("EnableImages", "true")
        .append_pair("EnableImageTypes", "Primary,Backdrop,Thumb")
        .append_pair("EnableUserData", "true")
        .append_pair("Fields", fields)
        .append_pair("Limit", &query.limit.to_string())
        .append_pair("Recursive", if query.recursive { "true" } else { "false" })
        .append_pair("SortOrder", query.sort_order.as_str())
        .append_pair("StartIndex", &query.start_index.to_string());
    if let Some(parent_id) = query.parent_id.as_deref() {
        pairs.append_pair("ParentId", parent_id);
    }
    if !ids.is_empty() {
        pairs.append_pair("Ids", &ids.join(","));
    }
    if !include_types.is_empty() {
        pairs.append_pair("IncludeItemTypes", &include_types);
    }
    if let Some(is_favorite) = query.is_favorite {
        pairs.append_pair("IsFavorite", if is_favorite { "true" } else { "false" });
    }
    if query.group_programs_by_series {
        pairs.append_pair("GroupProgramsBySeries", "true");
    }
    if let Some(search_term) = query
        .search_term
        .as_deref()
        .map(str::trim)
        .filter(|search_term| !search_term.is_empty())
    {
        pairs.append_pair("SearchTerm", search_term);
    }
    if let Some(sort_by) = query.sort_by {
        pairs.append_pair("SortBy", sort_by.as_str());
    }
}

fn add_latest_items_query(
    url: &mut url::Url,
    parent_id: &str,
    include_item_types: &[VideoItemType],
    limit: u32,
) {
    let include_types = item_types_query(include_item_types);
    let mut pairs = url.query_pairs_mut();
    pairs
        .append_pair("EnableImages", "true")
        .append_pair("EnableImageTypes", "Primary,Backdrop,Thumb")
        .append_pair("EnableUserData", "true")
        .append_pair("Fields", USER_ITEM_FIELDS)
        .append_pair("Limit", &limit.to_string())
        .append_pair("ParentId", parent_id);
    if !include_types.is_empty() {
        pairs.append_pair("IncludeItemTypes", &include_types);
    }
}

fn item_types_query(types: &[VideoItemType]) -> String {
    let mut seen = HashSet::new();
    types
        .iter()
        .copied()
        .filter(|item_type| seen.insert(*item_type))
        .map(VideoItemType::as_str)
        .collect::<Vec<_>>()
        .join(",")
}

fn stable_non_empty_ids(ids: &[String]) -> Vec<&str> {
    let mut seen = HashSet::new();
    ids.iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .filter(|id| seen.insert((*id).to_string()))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VideoItemType {
    Movie,
    Series,
    Episode,
}

impl VideoItemType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "Movie",
            Self::Series => "Series",
            Self::Episode => "Episode",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserItemsSort {
    SortName,
    DateLastContentAdded,
    DateCreated,
    PremiereDate,
    ProductionYear,
    CommunityRating,
    CriticRating,
    DatePlayed,
    PlayCount,
    Random,
    OfficialRating,
}

impl UserItemsSort {
    fn as_str(self) -> &'static str {
        match self {
            Self::SortName => "SortName",
            Self::DateLastContentAdded => "DateLastContentAdded,DateCreated,SortName",
            Self::DateCreated => "DateCreated,DateLastContentAdded,SortName",
            Self::PremiereDate => "PremiereDate",
            Self::ProductionYear => "ProductionYear",
            Self::CommunityRating => "CommunityRating",
            Self::CriticRating => "CriticRating",
            Self::DatePlayed => "DatePlayed",
            Self::PlayCount => "PlayCount",
            Self::Random => "Random",
            Self::OfficialRating => "OfficialRating",
        }
    }
}

#[derive(Clone, Debug)]
pub struct UserItemsQuery {
    pub parent_id: Option<String>,
    pub ids: Vec<String>,
    pub fields: Option<String>,
    pub include_item_types: Vec<VideoItemType>,
    pub is_favorite: Option<bool>,
    pub group_programs_by_series: bool,
    pub search_term: Option<String>,
    pub recursive: bool,
    pub start_index: u32,
    pub limit: u32,
    pub sort_by: Option<UserItemsSort>,
    pub sort_order: SortOrder,
}

impl Default for UserItemsQuery {
    fn default() -> Self {
        Self {
            parent_id: None,
            ids: Vec::new(),
            fields: None,
            include_item_types: Vec::new(),
            is_favorite: None,
            group_programs_by_series: false,
            search_term: None,
            recursive: true,
            start_index: 0,
            limit: 60,
            sort_by: None,
            sort_order: SortOrder::Ascending,
        }
    }
}

impl UserItemsQuery {
    fn validate(&self) -> Result<()> {
        if self.limit == 0 {
            bail!("Emby 项目查询 Limit 必须大于 0");
        }
        if let Some(parent_id) = self.parent_id.as_deref() {
            validate_non_empty_id(parent_id, "Emby 媒体库 ID")?;
        }
        if self
            .search_term
            .as_deref()
            .is_some_and(|search_term| search_term.trim().is_empty())
        {
            bail!("Emby 搜索词不能为空");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortOrder {
    Ascending,
    Descending,
}

impl SortOrder {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ascending => "Ascending",
            Self::Descending => "Descending",
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct AuthenticateUserByName<'a> {
    username: &'a str,
    password: &'a str,
    #[serde(rename = "Pw")]
    pw: &'a str,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AuthSession {
    pub user: Option<AuthUser>,
    pub session_info: Option<AuthSessionInfo>,
    pub access_token: String,
    pub server_id: Option<String>,
}

impl AuthSession {
    pub fn user_id(&self) -> Option<String> {
        self.user
            .as_ref()
            .and_then(|user| user.id.clone())
            .or_else(|| {
                self.session_info
                    .as_ref()
                    .and_then(|session| session.user_id.clone())
            })
    }

    pub fn user_name(&self) -> Option<String> {
        self.user
            .as_ref()
            .and_then(|user| user.name.clone())
            .or_else(|| {
                self.session_info
                    .as_ref()
                    .and_then(|session| session.user_name.clone())
            })
    }

    pub fn server_id(&self) -> Option<String> {
        self.server_id
            .clone()
            .or_else(|| self.user.as_ref().and_then(|user| user.server_id.clone()))
            .or_else(|| {
                self.session_info
                    .as_ref()
                    .and_then(|session| session.server_id.clone())
            })
    }

    pub fn server_name(&self) -> Option<String> {
        self.user
            .as_ref()
            .and_then(|user| user.server_name.clone())
            .or_else(|| {
                self.session_info
                    .as_ref()
                    .and_then(|session| session.id.clone())
            })
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AuthUser {
    pub name: Option<String>,
    pub server_id: Option<String>,
    pub server_name: Option<String>,
    pub id: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AuthSessionInfo {
    pub id: Option<String>,
    pub server_id: Option<String>,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserViews {
    pub items: Vec<UserView>,
    pub total_record_count: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserView {
    pub id: String,
    pub name: String,
    pub server_id: Option<String>,
    #[serde(rename = "Type")]
    pub item_type: Option<String>,
    pub collection_type: Option<String>,
    pub primary_image_aspect_ratio: Option<f64>,
    pub image_tags: Option<UserViewImageTags>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserViewImageTags {
    pub primary: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserItems {
    pub items: Vec<UserItem>,
    pub total_record_count: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserItem {
    pub id: String,
    pub name: String,
    #[serde(rename = "Type")]
    pub item_type: Option<String>,
    pub media_type: Option<String>,
    pub parent_id: Option<String>,
    pub series_id: Option<String>,
    pub series_name: Option<String>,
    pub index_number: Option<u32>,
    pub parent_index_number: Option<u32>,
    pub production_year: Option<u32>,
    pub community_rating: Option<f32>,
    pub image_tags: Option<HashMap<String, String>>,
    pub backdrop_image_tags: Option<Vec<String>>,
    pub user_data: Option<UserItemData>,
    pub collection_type: Option<String>,
    pub primary_image_aspect_ratio: Option<f64>,
    pub child_count: Option<u32>,
    pub container: Option<String>,
    pub can_delete: Option<bool>,
    pub provider_ids: Option<HashMap<String, String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserItemData {
    pub unplayed_item_count: Option<u32>,
    pub played_percentage: Option<f64>,
    pub playback_position_ticks: Option<u64>,
    #[serde(default)]
    pub is_favorite: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserItemImageSource<'a> {
    pub item_id: &'a str,
    pub image_type: EmbyImageType,
    pub tag: Option<&'a str>,
}

impl UserItem {
    pub fn primary_image_tag(&self) -> Option<&str> {
        self.image_tags
            .as_ref()
            .and_then(|tags| tags.get("Primary"))
            .map(String::as_str)
            .filter(|tag| !tag.trim().is_empty())
    }

    pub fn backdrop_image_tag(&self) -> Option<&str> {
        first_non_empty_tag(self.backdrop_image_tags.as_deref())
    }

    pub fn image_source(&self) -> UserItemImageSource<'_> {
        if let Some(tag) = self.primary_image_tag() {
            return UserItemImageSource {
                item_id: self.id.as_str(),
                image_type: EmbyImageType::Primary,
                tag: Some(tag),
            };
        }

        if let Some(tag) = self.backdrop_image_tag() {
            return UserItemImageSource {
                item_id: self.id.as_str(),
                image_type: EmbyImageType::Backdrop,
                tag: Some(tag),
            };
        }

        UserItemImageSource {
            item_id: self.id.as_str(),
            image_type: EmbyImageType::Primary,
            tag: None,
        }
    }

    pub fn episode_image_source(&self) -> UserItemImageSource<'_> {
        if let Some(tag) = self.backdrop_image_tag() {
            return UserItemImageSource {
                item_id: self.id.as_str(),
                image_type: EmbyImageType::Backdrop,
                tag: Some(tag),
            };
        }
        if let Some(tag) = self
            .image_tags
            .as_ref()
            .and_then(|tags| tags.get("Thumb"))
            .map(String::as_str)
            .filter(|tag| !tag.trim().is_empty())
        {
            return UserItemImageSource {
                item_id: self.id.as_str(),
                image_type: EmbyImageType::Thumb,
                tag: Some(tag),
            };
        }
        self.image_source()
    }

    pub fn unplayed_count(&self) -> Option<u32> {
        self.user_data
            .as_ref()
            .and_then(|data| data.unplayed_item_count)
            .filter(|count| *count > 0)
    }

    pub fn is_favorite(&self) -> bool {
        self.user_data.as_ref().is_some_and(|data| data.is_favorite)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ResumeItems {
    pub items: Vec<ResumeItem>,
    pub total_record_count: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ResumeItem {
    pub id: String,
    pub name: String,
    #[serde(rename = "Type")]
    pub item_type: Option<String>,
    pub parent_id: Option<String>,
    pub series_name: Option<String>,
    pub series_id: Option<String>,
    pub parent_index_number: Option<u32>,
    pub index_number: Option<u32>,
    pub production_year: Option<u32>,
    pub image_tags: Option<HashMap<String, String>>,
    pub backdrop_image_tags: Option<Vec<String>>,
    pub parent_backdrop_item_id: Option<String>,
    pub parent_backdrop_image_tags: Option<Vec<String>>,
    pub user_data: Option<UserItemData>,
}

pub struct ResumeItemImageSource<'a> {
    pub item_id: &'a str,
    pub image_type: EmbyImageType,
    pub tag: &'a str,
}

impl ResumeItem {
    pub fn is_favorite(&self) -> bool {
        self.user_data.as_ref().is_some_and(|data| data.is_favorite)
    }

    pub fn played_percentage(&self) -> Option<f64> {
        self.user_data
            .as_ref()
            .and_then(|data| data.played_percentage)
            .filter(|percentage| percentage.is_finite())
            .map(|percentage| percentage.clamp(0.0, 100.0))
    }

    pub fn primary_image_tag(&self) -> Option<&str> {
        self.image_tags
            .as_ref()
            .and_then(|tags| tags.get("Primary"))
            .map(String::as_str)
    }

    pub fn image_source(&self) -> Option<ResumeItemImageSource<'_>> {
        first_non_empty_tag(self.backdrop_image_tags.as_deref())
            .map(|tag| ResumeItemImageSource {
                item_id: self.id.as_str(),
                image_type: EmbyImageType::Backdrop,
                tag,
            })
            .or_else(|| {
                self.image_tags
                    .as_ref()
                    .and_then(|tags| tags.get("Primary"))
                    .map(|tag| ResumeItemImageSource {
                        item_id: self.id.as_str(),
                        image_type: EmbyImageType::Primary,
                        tag: tag.as_str(),
                    })
            })
            .or_else(|| {
                first_non_empty_tag(self.parent_backdrop_image_tags.as_deref()).and_then(|tag| {
                    self.parent_backdrop_item_id
                        .as_deref()
                        .map(|item_id| ResumeItemImageSource {
                            item_id,
                            image_type: EmbyImageType::Backdrop,
                            tag,
                        })
                })
            })
    }
}

fn first_non_empty_tag(tags: Option<&[String]>) -> Option<&str> {
    tags.and_then(|tags| {
        tags.iter()
            .map(String::as_str)
            .find(|tag| !tag.trim().is_empty())
    })
}

#[cfg(test)]
mod tests {
    use crate::server::{Protocol, ServerEndpoint};

    use super::*;

    #[test]
    fn parses_authentication_result() {
        let json = r#"
        {
            "AccessToken": "token",
            "ServerId": "server-1",
            "User": {
                "Id": "user-1",
                "Name": "luv",
                "ServerName": "Home"
            },
            "SessionInfo": {
                "Id": "session-1",
                "ServerId": "server-from-session",
                "UserId": "user-from-session",
                "UserName": "luv-session"
            }
        }
        "#;

        let session: AuthSession = serde_json::from_str(json).unwrap();

        assert_eq!(session.access_token, "token");
        assert_eq!(session.server_id().as_deref(), Some("server-1"));
        assert_eq!(session.server_name().as_deref(), Some("Home"));
        assert_eq!(session.user_id().as_deref(), Some("user-1"));
        assert_eq!(session.user_name().as_deref(), Some("luv"));
    }

    #[test]
    fn builds_authenticated_authorization_header() {
        let client = EmbyClient::new("device-1".to_string()).unwrap();

        let header = client.authenticated_authorization_header("token-1", "user-1");

        assert_eq!(
            header,
            "MediaBrowser Token=\"token-1\", UserId=\"user-1\", Client=\"Lenna\", Device=\"iPad\", DeviceId=\"device-1\", Version=\"1.0.13\""
        );
    }

    #[test]
    fn builds_user_views_url() {
        let endpoint = ServerEndpoint {
            protocol: Protocol::Https,
            address: "example.com".to_string(),
            port: 443,
            path: "/emby".to_string(),
        };
        let mut url = crate::emby::api_url(&endpoint, &["Users", "user-1", "Views"]).unwrap();
        url.query_pairs_mut()
            .append_pair("IncludeExternalContent", "false");

        assert_eq!(
            url.as_str(),
            "https://example.com/emby/Users/user-1/Views?IncludeExternalContent=false"
        );
    }

    #[test]
    fn parses_user_views() {
        let json = r#"
        {
            "Items": [
                {
                    "Name": "国漫",
                    "ServerId": "server-1",
                    "Id": "36089",
                    "Type": "CollectionFolder",
                    "CollectionType": "tvshows",
                    "PrimaryImageAspectRatio": 1.7777777777777777,
                    "ImageTags": {
                        "Primary": "bf3c9366aaf81c827488a5dffc88886e"
                    }
                },
                {
                    "Name": "华语电影",
                    "ServerId": "server-1",
                    "Id": "56259",
                    "Type": "CollectionFolder",
                    "CollectionType": "movies"
                }
            ],
            "TotalRecordCount": 2
        }
        "#;

        let views: UserViews = serde_json::from_str(json).unwrap();

        assert_eq!(views.total_record_count, 2);
        assert_eq!(views.items[0].id, "36089");
        assert_eq!(views.items[0].name, "国漫");
        assert_eq!(views.items[0].collection_type.as_deref(), Some("tvshows"));
        assert_eq!(
            views.items[0]
                .image_tags
                .as_ref()
                .and_then(|tags| tags.primary.as_deref()),
            Some("bf3c9366aaf81c827488a5dffc88886e")
        );
        assert_eq!(views.items[1].collection_type.as_deref(), Some("movies"));
    }

    #[test]
    fn builds_resume_items_url() {
        let endpoint = ServerEndpoint {
            protocol: Protocol::Https,
            address: "example.com".to_string(),
            port: 443,
            path: "/emby".to_string(),
        };
        let mut url =
            crate::emby::api_url(&endpoint, &["Users", "user-1", "Items", "Resume"]).unwrap();
        add_resume_items_query(&mut url);

        let pairs = url.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(
            pairs.get("EnableImages").map(|value| value.as_ref()),
            Some("true")
        );
        assert_eq!(
            pairs.get("EnableUserData").map(|value| value.as_ref()),
            Some("true")
        );
        let fields = pairs.get("Fields").unwrap();
        for field in [
            "ParentId",
            "SeriesId",
            "SeriesName",
            "IndexNumber",
            "ParentIndexNumber",
            "MediaType",
            "UserData",
        ] {
            assert!(fields.split(',').any(|candidate| candidate == field));
        }
        assert_eq!(pairs.get("Limit").map(|value| value.as_ref()), Some("30"));
    }

    #[test]
    fn serializes_library_sort_options() {
        assert_eq!(UserItemsSort::SortName.as_str(), "SortName");
        assert_eq!(
            UserItemsSort::DateLastContentAdded.as_str(),
            "DateLastContentAdded,DateCreated,SortName"
        );
        assert_eq!(
            UserItemsSort::DateCreated.as_str(),
            "DateCreated,DateLastContentAdded,SortName"
        );
        assert_eq!(UserItemsSort::PremiereDate.as_str(), "PremiereDate");
        assert_eq!(UserItemsSort::ProductionYear.as_str(), "ProductionYear");
        assert_eq!(UserItemsSort::CommunityRating.as_str(), "CommunityRating");
        assert_eq!(UserItemsSort::CriticRating.as_str(), "CriticRating");
        assert_eq!(UserItemsSort::DatePlayed.as_str(), "DatePlayed");
        assert_eq!(UserItemsSort::PlayCount.as_str(), "PlayCount");
        assert_eq!(UserItemsSort::Random.as_str(), "Random");
        assert_eq!(UserItemsSort::OfficialRating.as_str(), "OfficialRating");
    }

    #[test]
    fn builds_user_items_url() {
        let endpoint = ServerEndpoint {
            protocol: Protocol::Https,
            address: "example.com".to_string(),
            port: 443,
            path: "/emby".to_string(),
        };
        let mut url = crate::emby::api_url(&endpoint, &["Users", "user-1", "Items"]).unwrap();
        add_query_user_items_query(
            &mut url,
            &UserItemsQuery {
                parent_id: Some("36089".to_string()),
                include_item_types: vec![VideoItemType::Movie, VideoItemType::Series],
                start_index: 60,
                limit: 60,
                sort_by: Some(UserItemsSort::SortName),
                sort_order: SortOrder::Ascending,
                ..UserItemsQuery::default()
            },
        );

        let pairs = url.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(
            pairs.get("ParentId").map(|value| value.as_ref()),
            Some("36089")
        );
        assert_eq!(
            pairs.get("IncludeItemTypes").map(|value| value.as_ref()),
            Some("Movie,Series")
        );
        assert_eq!(
            pairs.get("StartIndex").map(|value| value.as_ref()),
            Some("60")
        );
        assert_eq!(pairs.get("Limit").map(|value| value.as_ref()), Some("60"));
        assert_eq!(
            pairs.get("SortBy").map(|value| value.as_ref()),
            Some("SortName")
        );
        assert_eq!(
            pairs.get("EnableUserData").map(|value| value.as_ref()),
            Some("true")
        );
    }

    #[test]
    fn builds_search_user_items_url() {
        let endpoint = ServerEndpoint {
            protocol: Protocol::Https,
            address: "example.com".to_string(),
            port: 443,
            path: "/emby".to_string(),
        };
        let query = search_user_items_query(" 你的名字 ", 0, 30).unwrap();
        let mut url = crate::emby::api_url(&endpoint, &["Users", "user-1", "Items"]).unwrap();
        add_query_user_items_query(&mut url, &query);

        assert_eq!(url.path(), "/emby/Users/user-1/Items");
        let pairs = url.query_pairs().collect::<HashMap<_, _>>();
        assert_eq!(
            pairs.get("EnableImageTypes").map(|value| value.as_ref()),
            Some("Primary,Backdrop,Thumb")
        );
        assert_eq!(
            pairs.get("Fields").map(|value| value.as_ref()),
            Some(SEARCH_USER_ITEM_FIELDS)
        );
        assert_eq!(
            pairs
                .get("GroupProgramsBySeries")
                .map(|value| value.as_ref()),
            Some("true")
        );
        assert_eq!(
            pairs.get("IncludeItemTypes").map(|value| value.as_ref()),
            Some("Movie,Series")
        );
        assert_eq!(pairs.get("Limit").map(|value| value.as_ref()), Some("30"));
        assert_eq!(
            pairs.get("Recursive").map(|value| value.as_ref()),
            Some("true")
        );
        assert_eq!(
            pairs.get("SearchTerm").map(|value| value.as_ref()),
            Some("你的名字")
        );
        assert_eq!(
            pairs.get("SortBy").map(|value| value.as_ref()),
            Some("DateCreated,DateLastContentAdded,SortName")
        );
        assert_eq!(
            pairs.get("SortOrder").map(|value| value.as_ref()),
            Some("Descending")
        );
    }

    #[test]
    fn user_items_ids_are_filtered_and_stably_deduplicated() {
        let endpoint = ServerEndpoint {
            protocol: Protocol::Https,
            address: "example.com".to_string(),
            port: 443,
            path: "/emby".to_string(),
        };
        let mut url = crate::emby::api_url(&endpoint, &["Users", "user-1", "Items"]).unwrap();
        add_query_user_items_query(
            &mut url,
            &UserItemsQuery {
                ids: vec!["b".into(), "".into(), "  a  ".into(), "b".into()],
                include_item_types: vec![VideoItemType::Movie, VideoItemType::Episode],
                limit: 40,
                ..UserItemsQuery::default()
            },
        );

        let ids = url.query_pairs().find(|(key, _)| key == "Ids").unwrap().1;
        assert_eq!(ids, "b,a");
    }

    #[test]
    fn builds_latest_items_url() {
        let endpoint = ServerEndpoint {
            protocol: Protocol::Https,
            address: "example.com".to_string(),
            port: 443,
            path: "/emby".to_string(),
        };
        let mut latest =
            crate::emby::api_url(&endpoint, &["Users", "user-1", "Items", "Latest"]).unwrap();
        add_latest_items_query(
            &mut latest,
            "view-1",
            &[VideoItemType::Series, VideoItemType::Episode],
            30,
        );
        assert_eq!(
            latest
                .query_pairs()
                .find(|(key, _)| key == "IncludeItemTypes")
                .unwrap()
                .1,
            "Series,Episode"
        );
        assert_eq!(
            latest
                .query_pairs()
                .find(|(key, _)| key == "ParentId")
                .unwrap()
                .1,
            "view-1"
        );
        assert_eq!(
            latest
                .query_pairs()
                .find(|(key, _)| key == "EnableUserData")
                .unwrap()
                .1,
            "true"
        );
    }

    #[test]
    fn builds_favorite_query_and_endpoint() {
        let endpoint = ServerEndpoint {
            protocol: Protocol::Https,
            address: "example.com".to_string(),
            port: 443,
            path: "/emby".to_string(),
        };
        let mut query_url = crate::emby::api_url(&endpoint, &["Users", "user-1", "Items"]).unwrap();
        add_query_user_items_query(
            &mut query_url,
            &UserItemsQuery {
                include_item_types: vec![VideoItemType::Movie, VideoItemType::Series],
                is_favorite: Some(true),
                sort_by: Some(UserItemsSort::SortName),
                ..UserItemsQuery::default()
            },
        );
        assert_eq!(
            query_url
                .query_pairs()
                .find(|(key, _)| key == "IsFavorite")
                .unwrap()
                .1,
            "true"
        );

        let favorite_url = favorite_item_url(&endpoint, "user-1", "item-1").unwrap();
        assert_eq!(
            favorite_url.as_str(),
            "https://example.com/emby/Users/user-1/FavoriteItems/item-1"
        );
        assert_eq!(favorite_method(true), Method::POST);
        assert_eq!(favorite_method(false), Method::DELETE);
    }

    #[test]
    fn builds_played_and_hide_from_resume_endpoints() {
        let endpoint = ServerEndpoint {
            protocol: Protocol::Https,
            address: "example.com".to_string(),
            port: 443,
            path: "/emby".to_string(),
        };

        let played = played_item_url(&endpoint, "user-1", "item-1").unwrap();
        assert_eq!(
            played.as_str(),
            "https://example.com/emby/Users/user-1/PlayedItems/item-1"
        );

        let hidden = hide_from_resume_url(&endpoint, "user-1", "item-1").unwrap();
        assert_eq!(
            hidden.as_str(),
            "https://example.com/emby/Users/user-1/Items/item-1/HideFromResume?Hide=true"
        );
    }

    #[test]
    fn parses_user_items() {
        let json = r#"
        {
            "Items": [
                {
                    "Id": "item-1",
                    "Name": "示例剧集",
                    "Type": "Series",
                    "ProductionYear": 2024,
                    "CommunityRating": 8.7,
                    "ImageTags": {
                        "Primary": "primary-tag-1"
                    },
                    "BackdropImageTags": [
                        "backdrop-tag-1"
                    ],
                    "UserData": {
                        "UnplayedItemCount": 12,
                        "IsFavorite": true
                    },
                    "CollectionType": "tvshows",
                    "PrimaryImageAspectRatio": 0.75,
                    "ChildCount": 24,
                    "Container": "mkv",
                    "CanDelete": false,
                    "ProviderIds": {
                        "Tmdb": "123"
                    }
                },
                {
                    "Id": "item-2",
                    "Name": "示例电影",
                    "Type": "Movie",
                    "ProductionYear": 2025,
                    "ImageTags": {
                        "Primary": "primary-tag-2"
                    },
                    "UserData": {
                        "UnplayedItemCount": 0
                    }
                },
                {
                    "Id": "item-3",
                    "Name": "只有背景图的电影",
                    "Type": "Movie",
                    "ProductionYear": 2026,
                    "ImageTags": {},
                    "BackdropImageTags": [
                        "backdrop-tag-3"
                    ]
                },
                {
                    "Id": "episode-4",
                    "Name": "第四集",
                    "Type": "Episode",
                    "MediaType": "Video",
                    "ParentId": "season-1",
                    "SeriesId": "series-1",
                    "SeriesName": "示例剧集",
                    "ParentIndexNumber": 1,
                    "IndexNumber": 4,
                    "ImageTags": {
                        "Thumb": "thumb-tag-4",
                        "Primary": "primary-tag-4"
                    }
                }
            ],
            "TotalRecordCount": 4
        }
        "#;

        let items: UserItems = serde_json::from_str(json).unwrap();

        assert_eq!(items.total_record_count, 4);
        assert_eq!(items.items[0].id, "item-1");
        assert_eq!(items.items[0].name, "示例剧集");
        assert_eq!(items.items[0].item_type.as_deref(), Some("Series"));
        assert_eq!(items.items[0].production_year, Some(2024));
        assert!((items.items[0].community_rating.unwrap() - 8.7).abs() < 0.001);
        assert_eq!(items.items[0].primary_image_tag(), Some("primary-tag-1"));
        assert_eq!(items.items[0].backdrop_image_tag(), Some("backdrop-tag-1"));
        let image_source = items.items[0].image_source();
        assert_eq!(image_source.item_id, "item-1");
        assert_eq!(image_source.image_type, EmbyImageType::Primary);
        assert_eq!(image_source.tag, Some("primary-tag-1"));
        assert_eq!(items.items[0].unplayed_count(), Some(12));
        assert!(items.items[0].is_favorite());
        assert_eq!(items.items[0].collection_type.as_deref(), Some("tvshows"));
        assert_eq!(items.items[0].primary_image_aspect_ratio, Some(0.75));
        assert_eq!(items.items[0].child_count, Some(24));
        assert_eq!(items.items[0].container.as_deref(), Some("mkv"));
        assert_eq!(items.items[0].can_delete, Some(false));
        assert_eq!(
            items.items[0]
                .provider_ids
                .as_ref()
                .and_then(|ids| ids.get("Tmdb"))
                .map(String::as_str),
            Some("123")
        );
        assert_eq!(items.items[1].primary_image_tag(), Some("primary-tag-2"));
        assert_eq!(items.items[1].unplayed_count(), None);
        assert!(!items.items[1].is_favorite());
        assert_eq!(items.items[2].primary_image_tag(), None);
        assert_eq!(items.items[2].backdrop_image_tag(), Some("backdrop-tag-3"));
        let image_source = items.items[2].image_source();
        assert_eq!(image_source.item_id, "item-3");
        assert_eq!(image_source.image_type, EmbyImageType::Backdrop);
        assert_eq!(image_source.tag, Some("backdrop-tag-3"));
        assert_eq!(items.items[3].media_type.as_deref(), Some("Video"));
        assert_eq!(items.items[3].parent_id.as_deref(), Some("season-1"));
        assert_eq!(items.items[3].series_id.as_deref(), Some("series-1"));
        assert_eq!(items.items[3].series_name.as_deref(), Some("示例剧集"));
        assert_eq!(items.items[3].parent_index_number, Some(1));
        assert_eq!(items.items[3].index_number, Some(4));
        let episode_source = items.items[3].episode_image_source();
        assert_eq!(episode_source.image_type, EmbyImageType::Thumb);
        assert_eq!(episode_source.tag, Some("thumb-tag-4"));
    }

    #[test]
    fn parses_favorite_user_item_data_response() {
        let data: UserItemData = serde_json::from_str(
            r#"{"UnplayedItemCount":2,"PlayedPercentage":25.0,"PlaybackPositionTicks":120000000,"IsFavorite":true}"#,
        )
        .unwrap();

        assert!(data.is_favorite);
        assert_eq!(data.unplayed_item_count, Some(2));
        assert_eq!(data.played_percentage, Some(25.0));
        assert_eq!(data.playback_position_ticks, Some(120_000_000));
    }

    #[test]
    fn validates_query_limits_and_ids() {
        let mut query = UserItemsQuery {
            limit: 0,
            ..UserItemsQuery::default()
        };
        assert!(query.validate().is_err());
        query.limit = 60;
        query.parent_id = Some("  ".to_string());
        assert!(query.validate().is_err());
        query.parent_id = None;
        query.search_term = Some("  ".to_string());
        assert!(query.validate().is_err());
        assert!(validate_non_empty_id(" ", "Emby 项目 ID").is_err());
    }

    #[test]
    fn video_item_types_serialize_to_emby_values() {
        assert_eq!(
            serde_json::to_string(&VideoItemType::Movie).unwrap(),
            "\"Movie\""
        );
        assert_eq!(
            serde_json::to_string(&VideoItemType::Series).unwrap(),
            "\"Series\""
        );
        assert_eq!(
            serde_json::to_string(&VideoItemType::Episode).unwrap(),
            "\"Episode\""
        );
    }

    #[test]
    fn parses_resume_items() {
        let json = r#"
        {
            "Items": [
                {
                    "Id": "episode-1",
                    "Name": "第一集",
                    "Type": "Episode",
                    "ParentId": "season-1",
                    "SeriesName": "示例剧集",
                    "SeriesId": "series-1",
                    "ParentIndexNumber": 1,
                    "IndexNumber": 3,
                    "ImageTags": {
                        "Primary": "episode-tag"
                    },
                    "BackdropImageTags": [
                        "episode-backdrop-tag"
                    ],
                    "UserData": {
                        "PlayedPercentage": 37.5,
                        "PlaybackPositionTicks": 9000000000
                    }
                },
                {
                    "Id": "movie-1",
                    "Name": "示例电影",
                    "Type": "Movie",
                    "ProductionYear": 2024,
                    "BackdropImageTags": [],
                    "ParentBackdropItemId": "movie-parent-1",
                    "ParentBackdropImageTags": [
                        "movie-parent-backdrop-tag"
                    ],
                    "ImageTags": {
                        "Primary": "movie-tag",
                        "Backdrop": "movie-backdrop-tag"
                    }
                },
                {
                    "Id": "movie-2",
                    "Name": "只有主图的电影",
                    "Type": "Movie",
                    "ProductionYear": 2025,
                    "BackdropImageTags": [],
                    "ParentBackdropImageTags": [],
                    "ImageTags": {
                        "Primary": "movie-primary-tag"
                    }
                },
                {
                    "Id": "movie-3",
                    "Name": "只有父级背景的电影",
                    "Type": "Movie",
                    "ProductionYear": 2026,
                    "BackdropImageTags": [],
                    "ParentBackdropItemId": "movie-parent-3",
                    "ParentBackdropImageTags": [
                        "movie-parent-3-backdrop-tag"
                    ],
                    "ImageTags": {}
                }
            ],
            "TotalRecordCount": 4
        }
        "#;

        let items: ResumeItems = serde_json::from_str(json).unwrap();

        assert_eq!(items.total_record_count, 4);
        assert_eq!(items.items[0].id, "episode-1");
        assert_eq!(items.items[0].item_type.as_deref(), Some("Episode"));
        assert_eq!(items.items[0].parent_id.as_deref(), Some("season-1"));
        assert_eq!(items.items[0].series_name.as_deref(), Some("示例剧集"));
        assert_eq!(items.items[0].series_id.as_deref(), Some("series-1"));
        assert_eq!(items.items[0].parent_index_number, Some(1));
        assert_eq!(items.items[0].index_number, Some(3));
        assert_eq!(items.items[0].played_percentage(), Some(37.5));
        assert_eq!(
            items.items[0]
                .user_data
                .as_ref()
                .and_then(|data| data.playback_position_ticks),
            Some(9_000_000_000)
        );
        assert_eq!(items.items[0].primary_image_tag(), Some("episode-tag"));
        let image_source = items.items[0].image_source().unwrap();
        assert_eq!(image_source.item_id, "episode-1");
        assert_eq!(image_source.image_type, EmbyImageType::Backdrop);
        assert_eq!(image_source.tag, "episode-backdrop-tag");
        assert_eq!(items.items[1].name, "示例电影");
        assert_eq!(items.items[1].item_type.as_deref(), Some("Movie"));
        assert_eq!(items.items[1].production_year, Some(2024));
        assert_eq!(items.items[1].primary_image_tag(), Some("movie-tag"));
        let image_source = items.items[1].image_source().unwrap();
        assert_eq!(image_source.item_id, "movie-1");
        assert_eq!(image_source.image_type, EmbyImageType::Primary);
        assert_eq!(image_source.tag, "movie-tag");
        assert_eq!(
            items.items[2].primary_image_tag(),
            Some("movie-primary-tag")
        );
        let image_source = items.items[2].image_source().unwrap();
        assert_eq!(image_source.item_id, "movie-2");
        assert_eq!(image_source.image_type, EmbyImageType::Primary);
        assert_eq!(image_source.tag, "movie-primary-tag");
        let image_source = items.items[3].image_source().unwrap();
        assert_eq!(image_source.item_id, "movie-parent-3");
        assert_eq!(image_source.image_type, EmbyImageType::Backdrop);
        assert_eq!(image_source.tag, "movie-parent-3-backdrop-tag");
    }
}
