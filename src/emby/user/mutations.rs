use super::*;

impl EmbyClient {
    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url(), item_id = %item_id, favorite))]
    pub fn set_favorite(
        &self,
        server: &CachedServer,
        item_id: &str,
        favorite: bool,
    ) -> Result<UserItemData> {
        let user_id = authenticated_user_id(server)?;
        validate_non_empty_id(item_id, "Emby 项目 ID")?;
        let url = favorite_item_url(&server.endpoint, user_id, item_id)?;
        let method = favorite_method(favorite);
        self.send_authenticated_json_url(server, method, url, "解析 Emby 收藏状态响应失败")
    }

    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url(), item_id = %item_id))]
    pub fn mark_item_played(&self, server: &CachedServer, item_id: &str) -> Result<UserItemData> {
        let user_id = authenticated_user_id(server)?;
        validate_non_empty_id(item_id, "Emby 项目 ID")?;
        let url = played_item_url(&server.endpoint, user_id, item_id)?;
        self.send_authenticated_json_url(server, Method::POST, url, "解析 Emby 已观看状态响应失败")
    }

    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url(), item_id = %item_id))]
    pub fn hide_item_from_resume(&self, server: &CachedServer, item_id: &str) -> Result<()> {
        let user_id = authenticated_user_id(server)?;
        validate_non_empty_id(item_id, "Emby 项目 ID")?;
        let url = hide_from_resume_url(&server.endpoint, user_id, item_id)?;
        self.send_authenticated_url(server, Method::POST, url)?;
        Ok(())
    }
}
