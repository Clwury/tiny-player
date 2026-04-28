use std::collections::HashMap;

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
        let user_id = server
            .user_id
            .as_deref()
            .ok_or_else(|| anyhow!("Emby 用户 ID 缺失，请重新登录服务器"))?;
        let mut url = api_url(&server.endpoint, &["Users", user_id, "Views"])?;
        url.query_pairs_mut()
            .append_pair("IncludeExternalContent", "false");
        let response_body = self.send_authenticated_url(server, Method::GET, url)?;

        serde_json::from_str::<UserViews>(&response_body).context("解析 Emby 用户视图响应失败")
    }

    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url()))]
    pub fn resume_items(&self, server: &CachedServer) -> Result<ResumeItems> {
        let user_id = server
            .user_id
            .as_deref()
            .ok_or_else(|| anyhow!("Emby 用户 ID 缺失，请重新登录服务器"))?;
        let mut url = api_url(&server.endpoint, &["Users", user_id, "Items", "Resume"])?;
        add_resume_items_query(&mut url);
        let response_body = self.send_authenticated_url(server, Method::GET, url)?;

        serde_json::from_str::<ResumeItems>(&response_body).context("解析 Emby 继续观看响应失败")
    }
}

fn add_resume_items_query(url: &mut url::Url) {
    url.query_pairs_mut()
        .append_pair("EnableImageTypes", "Primary,Backdrop,Thumb,Logo")
        .append_pair(
            "Fields",
            "BasicSyncInfo,Overview,Container,CanDelete,ProviderIds,ProductionYear,Genres,DateCreated,ParentId,People,ProductionYear,MediaSources,MediaStreams",
        )
        .append_pair("Limit", "30")
        .append_pair("MediaTypes", "Video")
        .append_pair("Recursive", "true");
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

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserViews {
    pub items: Vec<UserView>,
    pub total_record_count: u32,
}

#[derive(Clone, Debug, Deserialize)]
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

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserViewImageTags {
    pub primary: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ResumeItems {
    pub items: Vec<ResumeItem>,
    pub total_record_count: u32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ResumeItem {
    pub id: String,
    pub name: String,
    #[serde(rename = "Type")]
    pub item_type: Option<String>,
    pub series_name: Option<String>,
    pub parent_index_number: Option<u32>,
    pub index_number: Option<u32>,
    pub production_year: Option<u32>,
    pub image_tags: Option<HashMap<String, String>>,
    pub backdrop_image_tags: Option<Vec<String>>,
    pub parent_backdrop_item_id: Option<String>,
    pub parent_backdrop_image_tags: Option<Vec<String>>,
}

pub struct ResumeItemImageSource<'a> {
    pub item_id: &'a str,
    pub image_type: EmbyImageType,
    pub tag: &'a str,
}

impl ResumeItem {
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

        assert_eq!(
            url.as_str(),
            "https://example.com/emby/Users/user-1/Items/Resume?EnableImageTypes=Primary%2CBackdrop%2CThumb%2CLogo&Fields=BasicSyncInfo%2COverview%2CContainer%2CCanDelete%2CProviderIds%2CProductionYear%2CGenres%2CDateCreated%2CParentId%2CPeople%2CProductionYear%2CMediaSources%2CMediaStreams&Limit=30&MediaTypes=Video&Recursive=true"
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
                    "SeriesName": "示例剧集",
                    "ParentIndexNumber": 1,
                    "IndexNumber": 3,
                    "ImageTags": {
                        "Primary": "episode-tag"
                    },
                    "BackdropImageTags": [
                        "episode-backdrop-tag"
                    ]
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
        assert_eq!(items.items[0].series_name.as_deref(), Some("示例剧集"));
        assert_eq!(items.items[0].parent_index_number, Some(1));
        assert_eq!(items.items[0].index_number, Some(3));
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
