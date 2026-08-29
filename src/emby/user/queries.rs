use super::*;

impl EmbyClient {
    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url()))]
    pub fn user_views(&self, server: &CachedServer) -> Result<UserViews> {
        let user_id = authenticated_user_id(server)?;
        let mut url = api_url(&server.endpoint, &["Users", user_id, "Views"])?;
        url.query_pairs_mut()
            .append_pair("IncludeExternalContent", "false");
        self.send_authenticated_json_url(server, Method::GET, url, "解析 Emby 用户视图响应失败")
    }

    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url()))]
    pub fn resume_items(&self, server: &CachedServer) -> Result<ResumeItems> {
        let user_id = authenticated_user_id(server)?;
        let mut url = api_url(&server.endpoint, &["Users", user_id, "Items", "Resume"])?;
        add_resume_items_query(&mut url);
        self.send_authenticated_json_url(server, Method::GET, url, "解析 Emby 继续观看响应失败")
    }

    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url(), parent_id = %parent_id))]
    pub fn user_items(
        &self,
        server: &CachedServer,
        parent_id: &str,
        start_index: u32,
        limit: u32,
        sort_order: SortOrder,
    ) -> Result<UserItems> {
        self.query_user_items(
            server,
            &UserItemsQuery {
                parent_id: Some(parent_id.to_string()),
                include_item_types: vec![VideoItemType::Movie, VideoItemType::Series],
                start_index,
                limit,
                sort_by: Some(UserItemsSort::DateLastContentAdded),
                sort_order,
                ..UserItemsQuery::default()
            },
        )
    }

    #[instrument(skip(self, server, query), fields(server = %server.endpoint.display_url()))]
    pub fn query_user_items(
        &self,
        server: &CachedServer,
        query: &UserItemsQuery,
    ) -> Result<UserItems> {
        let user_id = authenticated_user_id(server)?;
        query.validate()?;
        let mut url = api_url(&server.endpoint, &["Users", user_id, "Items"])?;
        add_query_user_items_query(&mut url, query);
        self.send_authenticated_json_url(server, Method::GET, url, "解析 Emby 用户项目响应失败")
    }

    #[instrument(skip(self, server), fields(server = %server.endpoint.display_url(), search_term = %search_term))]
    pub fn search_items(
        &self,
        server: &CachedServer,
        search_term: &str,
        start_index: u32,
        limit: u32,
    ) -> Result<UserItems> {
        let query = search_user_items_query(search_term, start_index, limit)?;
        self.query_user_items(server, &query)
    }

    #[instrument(skip(self, server, include_item_types), fields(server = %server.endpoint.display_url(), parent_id = %parent_id))]
    pub fn latest_items(
        &self,
        server: &CachedServer,
        parent_id: &str,
        include_item_types: &[VideoItemType],
        limit: u32,
    ) -> Result<Vec<UserItem>> {
        let user_id = authenticated_user_id(server)?;
        validate_non_empty_id(parent_id, "Emby 媒体库 ID")?;
        if limit == 0 {
            bail!("Emby 项目查询 Limit 必须大于 0");
        }
        let mut url = api_url(&server.endpoint, &["Users", user_id, "Items", "Latest"])?;
        add_latest_items_query(&mut url, parent_id, include_item_types, limit);
        self.send_authenticated_json_url(server, Method::GET, url, "解析 Emby 最新项目响应失败")
    }
}
