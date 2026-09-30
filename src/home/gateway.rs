use crate::emby::UserItems;

#[cfg(test)]
pub(crate) mod test_support;

/// Home IO port. Methods return domain values, never entities or UI handles.
/// Additional Home endpoints move here as their controllers are migrated.
pub(super) trait HomeGateway: Send + Sync {
    fn similar_items(&self, item_id: &str) -> anyhow::Result<crate::emby::UserItems>;
    fn show_seasons(&self, series_id: &str) -> anyhow::Result<crate::emby::MediaItems>;
    fn show_next_up(&self, series_id: &str) -> anyhow::Result<crate::emby::MediaItems>;
    fn playback_media_sources(
        &self,
        item_id: &str,
    ) -> anyhow::Result<Vec<crate::emby::MediaSource>>;
    fn set_played(&self, item_id: &str, played: bool) -> anyhow::Result<crate::emby::UserItemData>;
    fn media_item(&self, item_id: &str) -> anyhow::Result<crate::emby::MediaItem>;
    fn show_episodes(
        &self,
        series_id: &str,
        season_id: Option<&str>,
    ) -> anyhow::Result<crate::emby::MediaItems>;

    fn mark_item_played(&self, item_id: &str) -> anyhow::Result<crate::emby::UserItemData>;
    fn hide_item_from_resume(&self, item_id: &str) -> anyhow::Result<()>;

    fn set_favorite(
        &self,
        item_id: &str,
        favorite: bool,
    ) -> anyhow::Result<crate::emby::UserItemData>;
    fn user_views(&self) -> anyhow::Result<crate::emby::UserViews>;
    fn resume_items(&self) -> anyhow::Result<crate::emby::ResumeItems>;
    fn latest_items(
        &self,
        view_id: &str,
        item_types: &[crate::emby::VideoItemType],
        limit: u32,
    ) -> anyhow::Result<Vec<crate::emby::UserItem>>;
    fn user_items(&self, query: &crate::emby::UserItemsQuery) -> anyhow::Result<UserItems>;
    fn search_items(&self, query: &str, start_index: u32, limit: u32) -> anyhow::Result<UserItems>;
}
