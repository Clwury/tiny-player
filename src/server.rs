use anyhow::Result;
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

    pub fn default_port(self) -> &'static str {
        match self {
            Self::Http => "8096",
            Self::Https => "443",
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
            "{}://{}:{}",
            self.protocol.scheme(),
            self.address,
            self.port
        ))?;
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
            self.address,
            self.port,
            path
        )
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
}
