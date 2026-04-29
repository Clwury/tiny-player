use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Method, blocking::Client};
use tracing::debug;

use crate::server::{CachedServer, ServerEndpoint};

pub mod image;
pub mod item;
pub mod system;
pub mod user;

pub use image::{EmbyImageRequest, EmbyImageType, ImageQuality};
pub use item::ItemCounts;
pub use system::PublicSystemInfo;
pub use user::{
    AuthSession, AuthSessionInfo, AuthUser, ResumeItem, ResumeItemImageSource, ResumeItems,
    SortOrder, UserItem, UserItemData, UserItems, UserView, UserViewImageTags, UserViews,
};

pub(super) const CLIENT_NAME: &str = "Lenna";
pub(super) const DEVICE_NAME: &str = "iPad";
pub(super) const VERSION: &str = "1.0.13";

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

    fn send_authenticated_request(
        &self,
        server: &CachedServer,
        method: Method,
        path_segments: &[&str],
    ) -> Result<String> {
        let url = api_url(&server.endpoint, path_segments)?;
        self.send_authenticated_url(server, method, url)
    }

    fn send_authenticated_url(
        &self,
        server: &CachedServer,
        method: Method,
        url: url::Url,
    ) -> Result<String> {
        let access_token = server
            .access_token
            .as_deref()
            .ok_or_else(|| anyhow!("Emby 访问令牌缺失，请重新登录服务器"))?;
        let user_id = server
            .user_id
            .as_deref()
            .ok_or_else(|| anyhow!("Emby 用户 ID 缺失，请重新登录服务器"))?;
        let authorization = self.authenticated_authorization_header(access_token, user_id);

        debug!(method = %method, url = %url, "sending authenticated Emby request");
        if log_secrets() {
            debug!(
                method = %method,
                url = %url,
                x_emby_authorization = %authorization,
                x_emby_token = %access_token,
                "full authenticated Emby request headers"
            );
        }

        let response = self
            .http
            .request(method, url)
            .header("Accept", "*/*")
            .header("Content-Type", "application/json")
            .header("X-Emby-Authorization", authorization)
            .header("X-Emby-Token", access_token)
            .header("User-Agent", format!("{CLIENT_NAME}/{VERSION}"))
            .send()
            .context("连接 Emby 服务器失败")?;

        let status = response.status();
        let response_headers = format!("{:?}", response.headers());
        let response_body = response.text().context("读取 Emby API 响应失败")?;
        debug!(status = %status, "received authenticated Emby response");
        if log_secrets() {
            debug!(
                status = %status,
                headers = %response_headers,
                body = %response_body,
                "full authenticated Emby response"
            );
        }

        if !status.is_success() {
            bail!("Emby API 请求失败：HTTP {status} {response_body}");
        }

        Ok(response_body)
    }

    fn send_authenticated_bytes_url(
        &self,
        server: &CachedServer,
        method: Method,
        url: url::Url,
    ) -> Result<Vec<u8>> {
        let access_token = server
            .access_token
            .as_deref()
            .ok_or_else(|| anyhow!("Emby 访问令牌缺失，请重新登录服务器"))?;
        let user_id = server
            .user_id
            .as_deref()
            .ok_or_else(|| anyhow!("Emby 用户 ID 缺失，请重新登录服务器"))?;
        let authorization = self.authenticated_authorization_header(access_token, user_id);

        debug!(method = %method, url = %url, "sending authenticated Emby image request");
        if log_secrets() {
            debug!(
                method = %method,
                url = %url,
                x_emby_authorization = %authorization,
                x_emby_token = %access_token,
                "full authenticated Emby image request headers"
            );
        }

        let response = self
            .http
            .request(method, url)
            .header("Accept", "image/*,*/*;q=0.8")
            .header("X-Emby-Authorization", authorization)
            .header("X-Emby-Token", access_token)
            .header("User-Agent", format!("{CLIENT_NAME}/{VERSION}"))
            .send()
            .context("连接 Emby 服务器失败")?;

        let status = response.status();
        let response_headers = format!("{:?}", response.headers());
        let response_bytes = response.bytes().context("读取 Emby 图片响应失败")?;
        debug!(status = %status, bytes = response_bytes.len(), "received authenticated Emby image response");
        if log_secrets() {
            debug!(
                status = %status,
                headers = %response_headers,
                bytes = response_bytes.len(),
                "full authenticated Emby image response metadata"
            );
        }

        if !status.is_success() {
            let body_preview = String::from_utf8_lossy(&response_bytes);
            let body_preview = body_preview.trim();
            if body_preview.is_empty() {
                bail!("Emby 图片请求失败：HTTP {status}");
            }
            let body_preview: String = body_preview.chars().take(256).collect();
            bail!("Emby 图片请求失败：HTTP {status} {body_preview}");
        }

        if response_bytes.is_empty() {
            bail!("Emby 图片响应为空");
        }

        Ok(response_bytes.to_vec())
    }

    fn authorization_header(&self) -> String {
        format!(
            "Emby UserId=\"\", Client=\"{CLIENT_NAME}\", Device=\"{DEVICE_NAME}\", DeviceId=\"{}\", Version=\"{VERSION}\", Token=\"\"",
            self.device_id
        )
    }

    fn authenticated_authorization_header(&self, access_token: &str, user_id: &str) -> String {
        format!(
            "MediaBrowser Token=\"{access_token}\", UserId=\"{user_id}\", Client=\"{CLIENT_NAME}\", Device=\"{DEVICE_NAME}\", DeviceId=\"{}\", Version=\"{VERSION}\"",
            self.device_id
        )
    }
}

fn log_secrets() -> bool {
    std::env::var_os("TINY_LOG_SECRETS").is_some_and(|value| value == "1")
}

fn api_url(endpoint: &ServerEndpoint, path_segments: &[&str]) -> Result<url::Url> {
    let mut url = endpoint.base_url()?;
    url.path_segments_mut()
        .map_err(|_| anyhow!("服务器地址不能作为 API 基础地址"))?
        .pop_if_empty()
        .extend(path_segments);

    Ok(url)
}
