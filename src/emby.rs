use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{
    Method,
    blocking::Client,
    header::{CONTENT_TYPE, HeaderMap},
};
use serde::{Serialize, de::DeserializeOwned};
use tracing::debug;

use crate::server::{CachedServer, ServerEndpoint};

pub mod image;
pub mod item;
pub mod playback;
pub mod show;
pub mod system;
pub mod user;

pub use image::{DownloadedImage, EmbyImageRequest, EmbyImageType, ImageQuality};
pub use item::ItemCounts;
pub use playback::{PlaybackInfo, PlaybackMediaSource};
pub use show::{
    MediaExternalUrl, MediaItem, MediaItems, MediaPerson, MediaSource, MediaStream, MediaStudio,
};
pub use system::PublicSystemInfo;
pub use user::{
    AuthSession, AuthSessionInfo, AuthUser, ResumeItem, ResumeItemImageSource, ResumeItems,
    SortOrder, UserItem, UserItemData, UserItemImageSource, UserItems, UserView, UserViewImageTags,
    UserViews,
};

pub(super) const CLIENT_NAME: &str = "Lenna";
pub(super) const DEVICE_NAME: &str = "iPad";
pub(super) const VERSION: &str = "1.0.13";

#[derive(Clone, Debug)]
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

    fn send_authenticated_json<T>(
        &self,
        server: &CachedServer,
        method: Method,
        path_segments: &[&str],
        parse_context: &'static str,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let response_body = self.send_authenticated_request(server, method, path_segments)?;
        serde_json::from_str::<T>(&response_body).context(parse_context)
    }

    fn send_authenticated_json_url<T>(
        &self,
        server: &CachedServer,
        method: Method,
        url: url::Url,
        parse_context: &'static str,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let response_body = self.send_authenticated_url(server, method, url)?;
        serde_json::from_str::<T>(&response_body).context(parse_context)
    }

    fn send_authenticated_json_body_url<T, B>(
        &self,
        server: &CachedServer,
        method: Method,
        url: url::Url,
        body: &B,
        parse_context: &'static str,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        B: Serialize + ?Sized,
    {
        let response_body = self.send_authenticated_body_url(server, method, url, body)?;
        serde_json::from_str::<T>(&response_body).context(parse_context)
    }

    fn send_public_json<T>(
        &self,
        endpoint: &ServerEndpoint,
        method: Method,
        path_segments: &[&str],
        http_error_prefix: &'static str,
        parse_context: &'static str,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let url = api_url(endpoint, path_segments)?;
        debug!(method = %method, url = %url, "sending Emby public request");
        let response = self
            .http
            .request(method, url)
            .header("User-Agent", format!("{CLIENT_NAME}/{VERSION}"))
            .send()
            .context("连接 Emby 服务器失败")?;

        let status = response.status();
        let response_headers = format!("{:?}", response.headers());
        let response_body = response.text().context("读取 Emby API 响应失败")?;
        debug!(status = %status, "received Emby public response");
        if log_secrets() {
            debug!(
                status = %status,
                headers = %response_headers,
                body = %response_body,
                "full Emby public response"
            );
        }

        if !status.is_success() {
            bail!("{http_error_prefix}：HTTP {status} {response_body}");
        }

        serde_json::from_str::<T>(&response_body).context(parse_context)
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

    fn send_authenticated_body_url<B>(
        &self,
        server: &CachedServer,
        method: Method,
        url: url::Url,
        body: &B,
    ) -> Result<String>
    where
        B: Serialize + ?Sized,
    {
        let access_token = server
            .access_token
            .as_deref()
            .ok_or_else(|| anyhow!("Emby 访问令牌缺失，请重新登录服务器"))?;
        let user_id = server
            .user_id
            .as_deref()
            .ok_or_else(|| anyhow!("Emby 用户 ID 缺失，请重新登录服务器"))?;
        let authorization = self.authenticated_authorization_header(access_token, user_id);
        let request_body = serde_json::to_string(body).context("序列化 Emby API 请求失败")?;

        debug!(method = %method, url = %url, "sending authenticated Emby request with body");
        if log_secrets() {
            debug!(
                method = %method,
                url = %url,
                x_emby_authorization = %authorization,
                x_emby_token = %access_token,
                body = %request_body,
                "full authenticated Emby request with body"
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
            .body(request_body)
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
    ) -> Result<DownloadedImage> {
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
        let content_type = content_type_header(response.headers());
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

        Ok(DownloadedImage {
            bytes: response_bytes.to_vec(),
            content_type,
        })
    }

    pub fn playback_http_headers(&self, server: &CachedServer) -> Result<Vec<(String, String)>> {
        let access_token = server
            .access_token
            .as_deref()
            .ok_or_else(|| anyhow!("Emby 访问令牌缺失，请重新登录服务器"))?;
        let user_id = server
            .user_id
            .as_deref()
            .ok_or_else(|| anyhow!("Emby 用户 ID 缺失，请重新登录服务器"))?;
        let authorization = self.authenticated_authorization_header(access_token, user_id);

        Ok(vec![
            ("Accept".to_string(), "*/*".to_string()),
            ("X-Emby-Authorization".to_string(), authorization),
            ("X-Emby-Token".to_string(), access_token.to_string()),
            ("User-Agent".to_string(), format!("{CLIENT_NAME}/{VERSION}")),
        ])
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

fn content_type_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let content_type = value.split(';').next()?.trim();
            (!content_type.is_empty()).then(|| content_type.to_ascii_lowercase())
        })
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
