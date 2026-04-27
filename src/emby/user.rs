use anyhow::{Context, Result, anyhow, bail};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument};

use crate::server::{AddServerSubmission, CachedServer};

use super::{CLIENT_NAME, EmbyClient, VERSION, api_url, log_secrets};

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
}
