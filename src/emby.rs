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
pub use playback::{
    PlaybackInfo, PlaybackMediaSource, PlaybackProgressReport, PlaybackQueueReportItem,
    PlaybackStartReport, PlaybackStopReport,
};
pub use show::{
    MediaExternalUrl, MediaGenre, MediaItem, MediaItems, MediaPerson, MediaSource, MediaStream,
    MediaStudio,
};
pub use system::PublicSystemInfo;
pub use user::{
    AuthSession, AuthSessionInfo, AuthUser, ResumeItem, ResumeItemImageSource, ResumeItems,
    SortOrder, UserItem, UserItemData, UserItemImageSource, UserItems, UserItemsQuery,
    UserItemsSort, UserView, UserViewImageTags, UserViews, VideoItemType,
};

const DEFAULT_CLIENT_NAME: &str = "Tiny Player";
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Debug)]
pub struct EmbyClient {
    client_name: String,
    device_id: String,
    device_name: String,
    http: Client,
}

impl EmbyClient {
    pub fn new(device_id: String) -> Result<Self> {
        Self::new_with_args(device_id, std::env::args_os().skip(1))
    }

    fn new_with_args(
        device_id: String,
        args: impl IntoIterator<Item = std::ffi::OsString>,
    ) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .context("创建 Emby HTTP 客户端失败")?;

        Ok(Self {
            client_name: client_name_from_args(args),
            device_id,
            device_name: device_name_from_system_name(sysinfo::System::name().as_deref()),
            http,
        })
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
        let include_secrets = log_secrets();
        debug!(method = %method, url = %url_for_log(&url, include_secrets), "sending Emby public request");
        let response = self
            .http
            .request(method, url)
            .header("User-Agent", self.user_agent())
            .send()
            .map_err(|error| anyhow!("连接 Emby 服务器失败：{}", error.without_url()))?;

        let status = response.status();
        let response_headers = include_secrets.then(|| format!("{:?}", response.headers()));
        let response_body = response.text().context("读取 Emby API 响应失败")?;
        debug!(status = %status, "received Emby public response");
        if let Some(response_headers) = response_headers {
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
        let include_secrets = log_secrets();

        debug!(method = %method, url = %url_for_log(&url, include_secrets), "sending authenticated Emby request");
        if include_secrets {
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
            .header("User-Agent", self.user_agent())
            .send()
            .map_err(|error| anyhow!("连接 Emby 服务器失败：{}", error.without_url()))?;

        let status = response.status();
        let response_headers = include_secrets.then(|| format!("{:?}", response.headers()));
        let response_body = response.text().context("读取 Emby API 响应失败")?;
        debug!(status = %status, "received authenticated Emby response");
        if let Some(response_headers) = response_headers {
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
        let include_secrets = log_secrets();

        debug!(method = %method, url = %url_for_log(&url, include_secrets), "sending authenticated Emby request with body");
        if include_secrets {
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
            .header("User-Agent", self.user_agent())
            .body(request_body)
            .send()
            .map_err(|error| anyhow!("连接 Emby 服务器失败：{}", error.without_url()))?;

        let status = response.status();
        let response_headers = include_secrets.then(|| format!("{:?}", response.headers()));
        let response_body = response.text().context("读取 Emby API 响应失败")?;
        debug!(status = %status, "received authenticated Emby response");
        if let Some(response_headers) = response_headers {
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
        let include_secrets = log_secrets();

        debug!(method = %method, url = %url_for_log(&url, include_secrets), "sending authenticated Emby image request");
        if include_secrets {
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
            .header("User-Agent", self.user_agent())
            .send()
            .map_err(|error| anyhow!("连接 Emby 服务器失败：{}", error.without_url()))?;

        let status = response.status();
        let content_type = content_type_header(response.headers());
        let response_headers = include_secrets.then(|| format!("{:?}", response.headers()));
        let response_bytes = response.bytes().context("读取 Emby 图片响应失败")?;
        debug!(status = %status, bytes = response_bytes.len(), "received authenticated Emby image response");
        if let Some(response_headers) = response_headers {
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
            ("User-Agent".to_string(), self.user_agent()),
        ])
    }

    fn authorization_header(&self) -> String {
        format!(
            "Emby UserId=\"\", Client=\"{}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{VERSION}\", Token=\"\"",
            self.client_name, self.device_name, self.device_id
        )
    }

    fn authenticated_authorization_header(&self, access_token: &str, user_id: &str) -> String {
        format!(
            "MediaBrowser Token=\"{access_token}\", UserId=\"{user_id}\", Client=\"{}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{VERSION}\"",
            self.client_name, self.device_name, self.device_id
        )
    }

    fn user_agent(&self) -> String {
        format!("{}/{VERSION}", self.client_name)
    }
}

fn client_name_from_args(args: impl IntoIterator<Item = std::ffi::OsString>) -> String {
    let mut args = args.into_iter().peekable();
    let mut name = None;
    while let Some(argument) = args.next() {
        if argument == "--" {
            break;
        }
        if argument == "--client-name" {
            name = args
                .next_if(|value| !value.to_string_lossy().starts_with('-'))
                .and_then(|value| value.into_string().ok());
        } else if let Some(value) = argument
            .to_str()
            .and_then(|argument| argument.strip_prefix("--client-name="))
        {
            name = Some(value.to_owned());
        }
    }
    name.as_deref()
        .map(str::trim)
        .filter(|name| {
            // Client is a quoted value in the comma-separated authorization header.
            !name.is_empty()
                && !name.chars().any(|character| {
                    character.is_control() || matches!(character, '"' | '\\' | ',')
                })
        })
        .unwrap_or(DEFAULT_CLIENT_NAME)
        .to_owned()
}

fn device_name_from_system_name(system_name: Option<&str>) -> String {
    system_name
        .map(str::trim)
        .filter(|name| {
            // Device is a quoted value in the comma-separated authorization header.
            !name.is_empty()
                && !name.chars().any(|character| {
                    character.is_control() || matches!(character, '"' | '\\' | ',')
                })
        })
        .unwrap_or(std::env::consts::OS)
        .to_owned()
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

const LOG_SECRETS_ENV: &str = "TINY_LOG_SECRETS";

fn log_secrets() -> bool {
    log_secrets_from_value(std::env::var_os(LOG_SECRETS_ENV).as_deref())
}

fn log_secrets_from_value(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| value == "1")
}

fn url_for_log(url: &url::Url, include_secrets: bool) -> String {
    if include_secrets {
        url.to_string()
    } else {
        redacted_url_for_log(url)
    }
}

fn redacted_url_for_log(url: &url::Url) -> String {
    let mut redacted = url.clone();
    if redacted.query().is_some() {
        let query = redacted
            .query_pairs()
            .map(|(name, value)| {
                let sensitive = matches!(
                    name.to_ascii_lowercase().as_str(),
                    "api_key" | "apikey" | "access_token" | "token"
                );
                (
                    name.into_owned(),
                    if sensitive {
                        "<redacted>".to_string()
                    } else {
                        value.into_owned()
                    },
                )
            })
            .collect::<Vec<_>>();
        redacted.set_query(None);
        redacted.query_pairs_mut().extend_pairs(query);
    }
    redacted.to_string()
}

fn api_url(endpoint: &ServerEndpoint, path_segments: &[&str]) -> Result<url::Url> {
    let mut url = endpoint.base_url()?;
    url.path_segments_mut()
        .map_err(|_| anyhow!("服务器地址不能作为 API 基础地址"))?
        .pop_if_empty()
        .extend(path_segments);

    Ok(url)
}

#[cfg(test)]
mod client_name_tests {
    use std::ffi::OsString;

    use super::{DEFAULT_CLIENT_NAME, EmbyClient, VERSION, client_name_from_args};

    fn parse(args: &[&str]) -> String {
        client_name_from_args(args.iter().map(OsString::from))
    }

    #[test]
    fn client_name_accepts_separate_and_equals_arguments() {
        assert_eq!(parse(&["--client-name", "My Player"]), "My Player");
        assert_eq!(parse(&["--client-name=My Player"]), "My Player");
        assert_eq!(
            parse(&["--client-name", "  自定义客户端  "]),
            "自定义客户端"
        );
        assert_eq!(
            parse(&["--client-name=First", "--client-name", "Second"]),
            "Second"
        );
    }

    #[test]
    fn client_name_falls_back_for_missing_empty_or_invalid_values() {
        for args in [
            vec![],
            vec!["--unrelated"],
            vec!["--client-name"],
            vec!["--client-name", "--unrelated"],
            vec!["--client-name="],
            vec!["--client-name", " \t "],
            vec!["--client-name", "Player\r\nOther: value"],
            vec!["--client-name", "Player\""],
            vec!["--client-name", "Player\\"],
            vec!["--client-name", "Player,Other"],
            vec!["--", "--client-name=Ignored"],
        ] {
            assert_eq!(parse(&args), DEFAULT_CLIENT_NAME, "args: {args:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn client_name_falls_back_for_non_unicode_values() {
        use std::os::unix::ffi::OsStringExt;

        assert_eq!(
            client_name_from_args([
                OsString::from("--client-name"),
                OsString::from_vec(vec![0xff])
            ]),
            DEFAULT_CLIENT_NAME
        );
    }

    #[test]
    fn custom_client_name_is_shared_by_authorization_and_user_agent() {
        let client = EmbyClient::new_with_args(
            "device-1".into(),
            ["--client-name", "My Player"].map(OsString::from),
        )
        .unwrap();

        assert!(
            client
                .authorization_header()
                .contains("Client=\"My Player\"")
        );
        assert!(
            client
                .authenticated_authorization_header("token", "user")
                .contains("Client=\"My Player\"")
        );
        assert_eq!(client.user_agent(), format!("My Player/{VERSION}"));
    }
}

#[cfg(test)]
mod device_name_tests {
    use super::device_name_from_system_name;

    #[test]
    fn device_name_preserves_runtime_system_names() {
        for name in ["Arch Linux", "Windows", "macOS", "Debian GNU/Linux"] {
            assert_eq!(device_name_from_system_name(Some(name)), name);
        }
        assert_eq!(
            device_name_from_system_name(Some("  Arch Linux  ")),
            "Arch Linux"
        );
    }

    #[test]
    fn device_name_falls_back_when_system_name_is_missing_or_invalid() {
        for name in [
            None,
            Some(""),
            Some(" \t "),
            Some("Linux\r\nOther: value"),
            Some("Linux\""),
            Some("Linux\\"),
            Some("Linux,Unix"),
        ] {
            assert_eq!(device_name_from_system_name(name), std::env::consts::OS);
        }
    }
}

#[cfg(test)]
mod log_safety_tests {
    use std::ffi::OsStr;

    use super::{log_secrets_from_value, url_for_log};

    #[test]
    fn authenticated_url_log_hides_credentials_by_default() {
        let url = url::Url::parse("https://example.test/video.mkv?api_key=secret-token&quality=90")
            .unwrap();

        let logged = url_for_log(&url, false);

        assert!(!logged.contains("secret-token"));
        assert!(logged.contains("quality=90"));
        assert!(logged.contains("api_key="));
    }

    #[test]
    fn authenticated_url_log_includes_credentials_when_explicitly_enabled() {
        let url = url::Url::parse("https://example.test/video.mkv?api_key=secret-token&quality=90")
            .unwrap();

        let logged = url_for_log(&url, true);

        assert!(logged.contains("secret-token"));
        assert!(logged.contains("quality=90"));
    }

    #[test]
    fn log_secrets_requires_explicit_one() {
        assert!(!log_secrets_from_value(None));
        assert!(!log_secrets_from_value(Some(OsStr::new(""))));
        assert!(!log_secrets_from_value(Some(OsStr::new("0"))));
        assert!(!log_secrets_from_value(Some(OsStr::new("true"))));
        assert!(log_secrets_from_value(Some(OsStr::new("1"))));
    }
}
