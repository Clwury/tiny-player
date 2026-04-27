use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument};

use crate::server::AddServerSubmission;

// const CLIENT_NAME: &str = "Tiny";
// const DEVICE_NAME: &str = "Linux";
const CLIENT_NAME: &str = "Lenna";
const DEVICE_NAME: &str = "iPad";
// const VERSION: &str = env!("CARGO_PKG_VERSION");
const VERSION: &str = "0.1.13";

pub struct EmbyClient {
    device_id: String,
    http: Client,
}

impl EmbyClient {
    pub fn new(device_id: String) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .context("创建 Emby HTTP 客户端失败")?;

        Ok(Self { device_id, http })
    }

    #[instrument(skip(self, submission), fields(server = %submission.endpoint.display_url()))]
    pub fn public_system_info(&self, submission: &AddServerSubmission) -> Result<PublicSystemInfo> {
        let mut url = submission.endpoint.base_url()?;
        url.path_segments_mut()
            .map_err(|_| anyhow!("服务器地址不能作为 API 基础地址"))?
            .pop_if_empty()
            .extend(["System", "Info", "Public"]);

        debug!(method = "GET", url = %url, "sending Emby public system info request");
        let response = self
            .http
            .get(url)
            .header("User-Agent", format!("{CLIENT_NAME}/{VERSION}"))
            .send()
            .context("连接 Emby 服务器失败")?;

        let status = response.status();
        let response_headers = format!("{:?}", response.headers());
        let response_body = response.text().context("读取 Emby 服务器信息响应失败")?;
        debug!(status = %status, "received Emby public system info response");
        if log_secrets() {
            debug!(
                status = %status,
                headers = %response_headers,
                body = %response_body,
                "full Emby public system info response"
            );
        }

        if !status.is_success() {
            bail!("获取 Emby 服务器信息失败：HTTP {status} {response_body}");
        }

        serde_json::from_str::<PublicSystemInfo>(&response_body)
            .context("解析 Emby 服务器信息响应失败")
    }

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

    fn authorization_header(&self) -> String {
        format!(
            "Emby UserId=\"\", Client=\"{CLIENT_NAME}\", Device=\"{DEVICE_NAME}\", DeviceId=\"{}\", Version=\"{VERSION}\", Token=\"\"",
            self.device_id
        )
    }
}

fn log_secrets() -> bool {
    std::env::var_os("TINY_LOG_SECRETS").is_some_and(|value| value == "1")
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
pub struct PublicSystemInfo {
    pub server_name: Option<String>,
    pub id: Option<String>,
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

#[cfg(test)]
mod tests {
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
}
