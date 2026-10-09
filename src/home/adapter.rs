use crate::{
    emby::{EmbyClient, UserItems},
    server::CachedServer,
};

use super::gateway::HomeGateway;

/// Immutable account/client snapshot owned by one effect. Request tokens govern
/// whether a response from this snapshot is allowed to reach the current model.
pub(crate) struct EmbyHomeGateway {
    pub(crate) client: EmbyClient,
    pub(crate) server: CachedServer,
}

impl HomeGateway for EmbyHomeGateway {
    fn similar_items(&self, item_id: &str) -> anyhow::Result<crate::emby::UserItems> {
        self.client.similar_items(&self.server, item_id)
    }
    fn show_seasons(&self, series_id: &str) -> anyhow::Result<crate::emby::MediaItems> {
        self.client.show_seasons(&self.server, series_id)
    }
    fn show_next_up(&self, series_id: &str) -> anyhow::Result<crate::emby::MediaItems> {
        self.client.show_next_up(&self.server, series_id)
    }
    fn playback_media_sources(
        &self,
        item_id: &str,
    ) -> anyhow::Result<Vec<crate::emby::MediaSource>> {
        self.client.playback_media_sources(&self.server, item_id)
    }
    fn set_played(&self, item_id: &str, played: bool) -> anyhow::Result<crate::emby::UserItemData> {
        self.client.set_played(&self.server, item_id, played)
    }
    fn media_item(&self, item_id: &str) -> anyhow::Result<crate::emby::MediaItem> {
        self.client.media_item(&self.server, item_id)
    }
    fn show_episodes(
        &self,
        series_id: &str,
        season_id: Option<&str>,
    ) -> anyhow::Result<crate::emby::MediaItems> {
        self.client
            .show_episodes(&self.server, series_id, season_id)
    }

    fn mark_item_played(&self, item_id: &str) -> anyhow::Result<crate::emby::UserItemData> {
        self.client.mark_item_played(&self.server, item_id)
    }
    fn hide_item_from_resume(&self, item_id: &str) -> anyhow::Result<()> {
        self.client.hide_item_from_resume(&self.server, item_id)
    }

    fn set_favorite(
        &self,
        item_id: &str,
        favorite: bool,
    ) -> anyhow::Result<crate::emby::UserItemData> {
        self.client.set_favorite(&self.server, item_id, favorite)
    }
    fn user_views(&self) -> anyhow::Result<crate::emby::UserViews> {
        self.client.user_views(&self.server)
    }
    fn resume_items(&self) -> anyhow::Result<crate::emby::ResumeItems> {
        self.client.resume_items(&self.server)
    }
    fn latest_items(
        &self,
        view_id: &str,
        item_types: &[crate::emby::VideoItemType],
        limit: u32,
    ) -> anyhow::Result<Vec<crate::emby::UserItem>> {
        self.client
            .latest_items(&self.server, view_id, item_types, limit)
    }
    fn user_items(&self, query: &crate::emby::UserItemsQuery) -> anyhow::Result<UserItems> {
        self.client.query_user_items(&self.server, query)
    }
    fn persons(&self, query: &crate::emby::UserItemsQuery) -> anyhow::Result<UserItems> {
        self.client.query_persons(&self.server, query)
    }
    fn search_items(&self, query: &str, start_index: u32, limit: u32) -> anyhow::Result<UserItems> {
        self.client
            .search_items(&self.server, query, start_index, limit)
    }
}
