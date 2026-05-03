use std::net::Ipv6Addr;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    Http,
    Https,
}

impl Protocol {
    pub fn scheme(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }

    pub fn from_scheme(scheme: &str) -> Option<Self> {
        match scheme {
            "http" => Some(Self::Http),
            "https" => Some(Self::Https),
            _ => None,
        }
    }

    pub fn default_port(self) -> &'static str {
        match self {
            Self::Http => "8096",
            Self::Https => "443",
        }
    }

    pub fn default_port_number(self) -> u16 {
        match self {
            Self::Http => 8096,
            Self::Https => 443,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Http => "HTTP",
            Self::Https => "HTTPS",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerEndpoint {
    pub protocol: Protocol,
    pub address: String,
    pub port: u16,
    pub path: String,
}

impl ServerEndpoint {
    pub fn parse_user_input(
        protocol: Protocol,
        address: &str,
        port: &str,
        path: &str,
    ) -> Result<Self> {
        let address = address.trim();
        if address.is_empty() {
            bail!("请输入服务器地址");
        }

        let fallback_port = parse_optional_port(port)?;
        if address.contains("://") {
            let url = Url::parse(address).context("服务器 URL 格式无效")?;
            return Self::from_url(url, fallback_port, path);
        }

        if let Ok(ipv6) = address.parse::<Ipv6Addr>() {
            return Ok(Self {
                protocol,
                address: ipv6.to_string(),
                port: fallback_port.unwrap_or_else(|| protocol.default_port_number()),
                path: normalize_path(path),
            });
        }

        let url = Url::parse(&format!("{}://{}", protocol.scheme(), address))
            .context("服务器地址格式无效")?;
        Self::from_url(url, fallback_port, path)
    }

    pub fn base_url(&self) -> Result<Url> {
        let mut path_segments = self
            .path
            .trim_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        if path_segments.last().copied() != Some("emby") {
            path_segments.push("emby");
        }

        let mut url = Url::parse(&format!(
            "{}://{}",
            self.protocol.scheme(),
            host_for_url(&self.address)
        ))?;
        url.set_port(Some(self.port))
            .map_err(|_| anyhow!("服务器端口无效：{}", self.port))?;
        url.set_path(&format!("/{}/", path_segments.join("/")));

        Ok(url)
    }

    pub fn display_url(&self) -> String {
        let path = if self.path.is_empty() {
            String::new()
        } else if self.path.starts_with('/') {
            self.path.clone()
        } else {
            format!("/{}", self.path)
        };

        format!(
            "{}://{}:{}{}",
            self.protocol.scheme(),
            host_for_url(&self.address),
            self.port,
            path
        )
    }

    pub fn address_input_value(&self) -> String {
        host_for_url(&self.address)
    }

    fn from_url(url: Url, fallback_port: Option<u16>, fallback_path: &str) -> Result<Self> {
        let protocol = Protocol::from_scheme(url.scheme())
            .ok_or_else(|| anyhow!("仅支持 HTTP/HTTPS 协议：{}", url.scheme()))?;
        if !url.username().is_empty() || url.password().is_some() {
            bail!("服务器地址不能包含用户名或密码");
        }
        if url.query().is_some() || url.fragment().is_some() {
            bail!("服务器地址不能包含查询参数或片段");
        }

        let host = url
            .host_str()
            .ok_or_else(|| anyhow!("服务器地址缺少 host"))?;
        if host.trim().is_empty() || host.chars().any(char::is_whitespace) {
            bail!("服务器地址 host 无效");
        }

        let url_path = normalize_path(url.path());
        let path = if url_path.is_empty() {
            normalize_path(fallback_path)
        } else {
            url_path
        };

        Ok(Self {
            protocol,
            address: normalize_host(host),
            port: url
                .port()
                .or(fallback_port)
                .unwrap_or_else(|| protocol.default_port_number()),
            path,
        })
    }
}

#[derive(Clone, Debug)]
pub struct AddServerSubmission {
    pub endpoint: ServerEndpoint,
    pub username: String,
    pub password: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedServer {
    pub id: String,
    pub endpoint: ServerEndpoint,
    pub username: String,
    pub password: String,
    pub user_id: Option<String>,
    pub server_id: Option<String>,
    pub server_name: Option<String>,
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_counts: Option<CachedItemCounts>,
    pub added_at_unix: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedItemCounts {
    pub movie_count: u32,
    pub series_count: u32,
}

fn parse_optional_port(port: &str) -> Result<Option<u16>> {
    let port = port.trim();
    if port.is_empty() {
        return Ok(None);
    }

    match port.parse::<u16>() {
        Ok(0) => bail!("端口必须大于 0"),
        Ok(port) => Ok(Some(port)),
        Err(_) => bail!("端口必须在 1-65535 之间"),
    }
}

fn normalize_path(path: &str) -> String {
    let path = path.trim().trim_matches('/');
    if path.is_empty() {
        String::new()
    } else {
        format!("/{path}")
    }
}

fn normalize_host(host: &str) -> String {
    host.trim_matches(['[', ']']).to_string()
}

fn host_for_url(host: &str) -> String {
    let host = normalize_host(host);
    if host.parse::<Ipv6Addr>().is_ok() {
        format!("[{host}]")
    } else {
        host
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_base_url_with_empty_path() {
        let endpoint = ServerEndpoint {
            protocol: Protocol::Https,
            address: "example.com".to_string(),
            port: 443,
            path: String::new(),
        };

        assert_eq!(
            endpoint.base_url().unwrap().as_str(),
            "https://example.com/emby/"
        );
    }

    #[test]
    fn builds_base_url_with_nested_path() {
        let endpoint = ServerEndpoint {
            protocol: Protocol::Http,
            address: "127.0.0.1".to_string(),
            port: 8096,
            path: "/emby".to_string(),
        };

        assert_eq!(
            endpoint.base_url().unwrap().as_str(),
            "http://127.0.0.1:8096/emby/"
        );
    }

    #[test]
    fn appends_emby_after_custom_path() {
        let endpoint = ServerEndpoint {
            protocol: Protocol::Https,
            address: "example.com".to_string(),
            port: 443,
            path: "/custom".to_string(),
        };

        assert_eq!(
            endpoint.base_url().unwrap().as_str(),
            "https://example.com/custom/emby/"
        );
    }

    #[test]
    fn builds_base_url_for_ipv6_host() {
        let endpoint = ServerEndpoint {
            protocol: Protocol::Http,
            address: "::1".to_string(),
            port: 8096,
            path: String::new(),
        };

        assert_eq!(
            endpoint.base_url().unwrap().as_str(),
            "http://[::1]:8096/emby/"
        );
        assert_eq!(endpoint.display_url(), "http://[::1]:8096");
    }

    #[test]
    fn parses_plain_host_with_defaults() {
        let endpoint =
            ServerEndpoint::parse_user_input(Protocol::Https, " example.com ", "", "").unwrap();

        assert_eq!(endpoint.protocol, Protocol::Https);
        assert_eq!(endpoint.address, "example.com");
        assert_eq!(endpoint.port, 443);
        assert_eq!(endpoint.path, "");
        assert_eq!(
            endpoint.base_url().unwrap().as_str(),
            "https://example.com/emby/"
        );
    }

    #[test]
    fn parses_complete_url_with_path_and_port() {
        let endpoint = ServerEndpoint::parse_user_input(
            Protocol::Https,
            "http://example.com:8096/library",
            "443",
            "/ignored",
        )
        .unwrap();

        assert_eq!(endpoint.protocol, Protocol::Http);
        assert_eq!(endpoint.address, "example.com");
        assert_eq!(endpoint.port, 8096);
        assert_eq!(endpoint.path, "/library");
        assert_eq!(
            endpoint.base_url().unwrap().as_str(),
            "http://example.com:8096/library/emby/"
        );
    }

    #[test]
    fn parses_host_with_inline_port_and_path() {
        let endpoint = ServerEndpoint::parse_user_input(
            Protocol::Https,
            "example.com:8920/jellyfin",
            "443",
            "",
        )
        .unwrap();

        assert_eq!(endpoint.protocol, Protocol::Https);
        assert_eq!(endpoint.address, "example.com");
        assert_eq!(endpoint.port, 8920);
        assert_eq!(endpoint.path, "/jellyfin");
        assert_eq!(
            endpoint.base_url().unwrap().as_str(),
            "https://example.com:8920/jellyfin/emby/"
        );
    }

    #[test]
    fn parses_ipv6_inputs() {
        let bare = ServerEndpoint::parse_user_input(Protocol::Http, "::1", "8096", "").unwrap();
        assert_eq!(bare.address, "::1");
        assert_eq!(bare.base_url().unwrap().as_str(), "http://[::1]:8096/emby/");

        let url = ServerEndpoint::parse_user_input(
            Protocol::Https,
            "http://[2001:db8::1]:8096/custom",
            "443",
            "",
        )
        .unwrap();
        assert_eq!(url.protocol, Protocol::Http);
        assert_eq!(url.address, "2001:db8::1");
        assert_eq!(url.port, 8096);
        assert_eq!(url.path, "/custom");
        assert_eq!(
            url.base_url().unwrap().as_str(),
            "http://[2001:db8::1]:8096/custom/emby/"
        );
    }

    #[test]
    fn rejects_invalid_endpoint_inputs() {
        assert!(ServerEndpoint::parse_user_input(Protocol::Https, " ", "443", "").is_err());
        assert!(
            ServerEndpoint::parse_user_input(Protocol::Https, "ftp://example.com", "443", "")
                .is_err()
        );
        assert!(
            ServerEndpoint::parse_user_input(Protocol::Https, "https://bad host", "443", "")
                .is_err()
        );
        assert!(
            ServerEndpoint::parse_user_input(Protocol::Https, "example.com", "70000", "").is_err()
        );
        assert!(
            ServerEndpoint::parse_user_input(Protocol::Https, "user:pass@example.com", "443", "")
                .is_err()
        );
    }
}
