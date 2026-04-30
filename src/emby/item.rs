use anyhow::Result;
use reqwest::Method;
use serde::Deserialize;
use tracing::instrument;

use crate::server::CachedServer;

use super::EmbyClient;

impl EmbyClient {
    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url()))]
    pub fn item_counts(&self, server: &CachedServer) -> Result<ItemCounts> {
        self.send_authenticated_json(
            server,
            Method::GET,
            &["Items", "Counts"],
            "解析 Emby 媒体数量响应失败",
        )
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ItemCounts {
    pub movie_count: u32,
    pub series_count: u32,
    pub episode_count: u32,
    pub game_count: u32,
    pub artist_count: u32,
    pub program_count: u32,
    pub game_system_count: u32,
    pub trailer_count: u32,
    pub song_count: u32,
    pub album_count: u32,
    pub music_video_count: u32,
    pub box_set_count: u32,
    pub book_count: u32,
    pub item_count: u32,
}

impl From<&crate::server::CachedItemCounts> for ItemCounts {
    fn from(counts: &crate::server::CachedItemCounts) -> Self {
        Self {
            movie_count: counts.movie_count,
            series_count: counts.series_count,
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::server::{Protocol, ServerEndpoint};

    use super::*;

    #[test]
    fn builds_items_counts_url() {
        let endpoint = ServerEndpoint {
            protocol: Protocol::Https,
            address: "example.com".to_string(),
            port: 443,
            path: "/emby".to_string(),
        };

        let url = crate::emby::api_url(&endpoint, &["Items", "Counts"]).unwrap();

        assert_eq!(url.as_str(), "https://example.com/emby/Items/Counts");
    }

    #[test]
    fn parses_item_counts() {
        let json = r#"
        {
            "MovieCount": 16417,
            "SeriesCount": 16395,
            "EpisodeCount": 245139,
            "GameCount": 0,
            "ArtistCount": 0,
            "ProgramCount": 0,
            "GameSystemCount": 0,
            "TrailerCount": 0,
            "SongCount": 0,
            "AlbumCount": 0,
            "MusicVideoCount": 0,
            "BoxSetCount": 0,
            "BookCount": 0,
            "ItemCount": 0
        }
        "#;

        let counts: ItemCounts = serde_json::from_str(json).unwrap();

        assert_eq!(counts.movie_count, 16417);
        assert_eq!(counts.series_count, 16395);
        assert_eq!(counts.episode_count, 245139);
    }
}
