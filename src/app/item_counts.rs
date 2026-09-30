use super::TinyApp;
use crate::{
    emby::ItemCounts,
    server::feature::{CountRequest, CountResult, gateway::ServerGateway},
};
use anyhow::Result;
use gpui::{AppContext as _, Context};

impl TinyApp {
    pub(super) fn refresh_saved_server_counts(&mut self, server_id: &str, cx: &mut Context<Self>) {
        self.server_feature.reset_counts(server_id);
        self.server_effects.counts.remove(server_id);
        self.load_item_counts_for_server_id(server_id, cx);
    }

    pub(super) fn load_item_counts_for_server_id(
        &mut self,
        server_id: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = self.server_feature.begin_counts(server_id) else {
            return;
        };
        let Some(client) = self.emby_client.clone() else {
            self.server_feature.reset_counts(server_id);
            self.push_app_error_notification("Emby HTTP 客户端不可用", cx);
            cx.notify();
            return;
        };
        let task_server = request.server.clone();
        let task =
            cx.background_spawn(async move { ServerGateway::item_counts(&client, &task_server) });
        let delivery = cx.spawn(async move |app, cx| {
            let result = task.await;
            app.update(cx, |app, cx| app.finish_item_counts(request, result, cx))
                .ok();
        });
        self.server_effects
            .counts
            .entry(server_id.into())
            .or_default()
            .replace(delivery);
    }

    fn finish_item_counts(
        &mut self,
        request: CountRequest,
        result: Result<ItemCounts>,
        cx: &mut Context<Self>,
    ) {
        match self.server_feature.finish_counts(&request, result) {
            CountResult::Ignored => return,
            CountResult::Saved => self.schedule_cache_save("保存媒体数量缓存失败", cx),
            CountResult::Failed => {}
        }
        self.server_effects.counts.remove(&request.server.id);
        cx.notify();
    }

    pub(super) fn retain_item_count_state(&mut self) {
        self.server_feature.retain_counts();
        self.server_effects
            .counts
            .retain(|id, _| self.server_feature.counts_loading(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::CachedServer;
    use crate::{storage::ServerCache, theme};
    use gpui::TestAppContext;

    #[gpui::test]
    fn stale_count_responses_cannot_replace_counts_after_an_edit(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        cx.update(theme::init);
        let server: CachedServer = serde_json::from_value(serde_json::json!({
            "id": "server", "endpoint": {"protocol": "Https", "address": "example.com", "port": 443, "path": ""},
            "username": "new-user", "password": "", "user_id": "new-user", "access_token": "new-token", "added_at_unix": 0
        })).unwrap();
        let app = cx.new(|cx| {
            let mut cache = ServerCache::empty();
            cache.servers.push(server.clone());
            let mut app = TinyApp::new(cache, None, cx);
            app.cache_save_path = Some(temp.path().join("servers.json"));
            app
        });
        app.update(cx, |app, cx| {
            let request = app.server_feature.begin_counts(&server.id).unwrap();
            let mut previous = server.clone();
            previous.access_token = Some("old-token".into());
            app.finish_item_counts(
                CountRequest {
                    server: previous,
                    token: request.token.clone(),
                },
                Ok(ItemCounts {
                    movie_count: 999,
                    series_count: 999,
                    ..Default::default()
                }),
                cx,
            );
            assert!(app.server_feature.counts_loading(&server.id));
            assert!(
                !app.server_feature
                    .test_state()
                    .counts
                    .contains_key(&server.id)
            );
            assert!(
                app.server_feature.catalog().servers[0]
                    .item_counts
                    .is_none()
            );

            app.finish_item_counts(
                request.clone(),
                Ok(ItemCounts {
                    movie_count: 12,
                    series_count: 34,
                    ..Default::default()
                }),
                cx,
            );
            assert!(!app.server_feature.counts_loading(&server.id));
            assert_eq!(
                app.server_feature.test_state().counts[&server.id].movie_count,
                12
            );
            assert_eq!(
                app.server_feature.catalog().servers[0]
                    .item_counts
                    .as_ref()
                    .unwrap()
                    .series_count,
                34
            );

            // Saving an edit keeps the old display, even when credentials
            // are unchanged, but prevents using that login for new requests.
            app.server_feature.test_catalog_mut().servers[0].needs_auth_refresh = true;
            app.retain_item_count_state();
            app.load_item_counts_for_server_id(&server.id, cx);
            assert!(!app.server_feature.counts_loading(&server.id));
            assert!(
                !app.server_feature
                    .test_state()
                    .counts_refreshed
                    .contains(&server.id)
            );
            app.finish_item_counts(
                request.clone(),
                Ok(ItemCounts {
                    movie_count: 999,
                    series_count: 999,
                    ..Default::default()
                }),
                cx,
            );
            assert_eq!(
                app.server_feature.test_state().counts[&server.id].movie_count,
                12
            );
            assert_eq!(
                app.server_feature.catalog().servers[0]
                    .item_counts
                    .as_ref()
                    .unwrap()
                    .series_count,
                34
            );
        });
    }
}
