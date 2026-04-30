use anyhow::{Result, anyhow, bail};
use reqwest::Method;

use crate::server::CachedServer;

use super::{EmbyClient, api_url};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadedImage {
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EmbyImageType {
    Primary,
    Art,
    Backdrop,
    Banner,
    Logo,
    Thumb,
    Disc,
    Box,
    Screenshot,
    Menu,
    Chapter,
    BoxRear,
    Thumbnail,
}

impl EmbyImageType {
    pub fn as_path_segment(self) -> &'static str {
        match self {
            Self::Primary => "Primary",
            Self::Art => "Art",
            Self::Backdrop => "Backdrop",
            Self::Banner => "Banner",
            Self::Logo => "Logo",
            Self::Thumb => "Thumb",
            Self::Disc => "Disc",
            Self::Box => "Box",
            Self::Screenshot => "Screenshot",
            Self::Menu => "Menu",
            Self::Chapter => "Chapter",
            Self::BoxRear => "BoxRear",
            Self::Thumbnail => "Thumbnail",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImageQuality(u8);

impl ImageQuality {
    pub const DEFAULT: Self = Self(90);

    pub fn new(value: u8) -> Result<Self> {
        if value > 100 {
            bail!("Emby 图片质量必须在 0 到 100 之间：{value}");
        }

        Ok(Self(value))
    }

    pub fn get(self) -> u8 {
        self.0
    }
}

impl Default for ImageQuality {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EmbyImageRequest {
    pub item_id: String,
    pub image_type: EmbyImageType,
    pub max_width: Option<u32>,
    pub tag: Option<String>,
    pub quality: ImageQuality,
}

impl EmbyImageRequest {
    pub fn new(item_id: impl Into<String>, image_type: EmbyImageType) -> Self {
        Self {
            item_id: item_id.into(),
            image_type,
            max_width: None,
            tag: None,
            quality: ImageQuality::DEFAULT,
        }
    }

    pub fn primary(item_id: impl Into<String>, tag: Option<String>) -> Self {
        Self::new(item_id, EmbyImageType::Primary).with_tag(tag)
    }

    pub fn with_max_width(mut self, max_width: u32) -> Self {
        self.max_width = Some(max_width);
        self
    }

    pub fn with_tag(mut self, tag: Option<String>) -> Self {
        self.tag = tag.and_then(|tag| {
            let tag = tag.trim().to_string();
            (!tag.is_empty()).then_some(tag)
        });
        self
    }

    pub fn with_quality(mut self, quality: ImageQuality) -> Self {
        self.quality = quality;
        self
    }

    fn validate(&self) -> Result<()> {
        if self.item_id.trim().is_empty() {
            return Err(anyhow!("Emby 图片项目 ID 不能为空"));
        }

        if self.max_width == Some(0) {
            return Err(anyhow!("Emby 图片最大宽度必须大于 0"));
        }

        Ok(())
    }
}

impl EmbyClient {
    pub fn image_url(&self, server: &CachedServer, request: &EmbyImageRequest) -> Result<url::Url> {
        request.validate()?;

        let mut url = api_url(
            &server.endpoint,
            &[
                "Items",
                request.item_id.as_str(),
                "Images",
                request.image_type.as_path_segment(),
            ],
        )?;
        {
            let mut query = url.query_pairs_mut();
            if let Some(max_width) = request.max_width {
                query.append_pair("maxWidth", &max_width.to_string());
            }
            if let Some(tag) = request.tag.as_deref() {
                query.append_pair("tag", tag);
            }
            query.append_pair("quality", &request.quality.get().to_string());
        }

        Ok(url)
    }

    pub fn item_image(
        &self,
        server: &CachedServer,
        request: &EmbyImageRequest,
    ) -> Result<DownloadedImage> {
        let url = self.image_url(server, request)?;
        self.send_authenticated_bytes_url(server, Method::GET, url)
    }
}

#[cfg(test)]
mod tests {
    use crate::server::{CachedServer, Protocol, ServerEndpoint};

    use super::*;

    fn server() -> CachedServer {
        CachedServer {
            id: "server-local".to_string(),
            endpoint: ServerEndpoint {
                protocol: Protocol::Https,
                address: "example.com".to_string(),
                port: 443,
                path: "/emby".to_string(),
            },
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
    fn maps_image_types_to_path_segments() {
        let mappings = [
            (EmbyImageType::Primary, "Primary"),
            (EmbyImageType::Art, "Art"),
            (EmbyImageType::Backdrop, "Backdrop"),
            (EmbyImageType::Banner, "Banner"),
            (EmbyImageType::Logo, "Logo"),
            (EmbyImageType::Thumb, "Thumb"),
            (EmbyImageType::Disc, "Disc"),
            (EmbyImageType::Box, "Box"),
            (EmbyImageType::Screenshot, "Screenshot"),
            (EmbyImageType::Menu, "Menu"),
            (EmbyImageType::Chapter, "Chapter"),
            (EmbyImageType::BoxRear, "BoxRear"),
            (EmbyImageType::Thumbnail, "Thumbnail"),
        ];

        for (image_type, segment) in mappings {
            assert_eq!(image_type.as_path_segment(), segment);
        }
    }

    #[test]
    fn validates_image_quality() {
        assert_eq!(ImageQuality::new(0).unwrap().get(), 0);
        assert_eq!(ImageQuality::new(90).unwrap().get(), 90);
        assert_eq!(ImageQuality::new(100).unwrap().get(), 100);
        assert!(ImageQuality::new(101).is_err());
    }

    #[test]
    fn builds_image_url_with_all_parameters() {
        let client = EmbyClient::new("device-1".to_string()).unwrap();
        let request = EmbyImageRequest::primary(
            "36089",
            Some("bf3c9366aaf81c827488a5dffc88886e".to_string()),
        )
        .with_max_width(640);

        let url = client.image_url(&server(), &request).unwrap();

        assert_eq!(
            url.as_str(),
            "https://example.com/emby/Items/36089/Images/Primary?maxWidth=640&tag=bf3c9366aaf81c827488a5dffc88886e&quality=90"
        );
    }

    #[test]
    fn builds_image_url_without_optional_tag() {
        let client = EmbyClient::new("device-1".to_string()).unwrap();
        let request = EmbyImageRequest::primary("36089", None).with_max_width(640);

        let url = client.image_url(&server(), &request).unwrap();

        assert_eq!(
            url.as_str(),
            "https://example.com/emby/Items/36089/Images/Primary?maxWidth=640&quality=90"
        );
    }

    #[test]
    fn builds_backdrop_image_url() {
        let client = EmbyClient::new("device-1".to_string()).unwrap();
        let request = EmbyImageRequest::new("1033990", EmbyImageType::Backdrop)
            .with_max_width(800)
            .with_tag(Some("0cc5f0570829e912c9575f11db97744a".to_string()));

        let url = client.image_url(&server(), &request).unwrap();

        assert_eq!(
            url.as_str(),
            "https://example.com/emby/Items/1033990/Images/Backdrop?maxWidth=800&tag=0cc5f0570829e912c9575f11db97744a&quality=90"
        );
    }

    #[test]
    fn builds_image_url_without_max_width() {
        let client = EmbyClient::new("device-1".to_string()).unwrap();
        let request = EmbyImageRequest::primary("36089", Some("abc".to_string()));

        let url = client.image_url(&server(), &request).unwrap();

        assert_eq!(
            url.as_str(),
            "https://example.com/emby/Items/36089/Images/Primary?tag=abc&quality=90"
        );
    }

    #[test]
    fn rejects_empty_item_id() {
        let client = EmbyClient::new("device-1".to_string()).unwrap();
        let request = EmbyImageRequest::primary(" ", Some("abc".to_string()));

        assert!(client.image_url(&server(), &request).is_err());
    }
}
