use std::collections::{HashMap, HashSet};

use anyhow::Result;
use gpui::{AppContext as _, Context};

use crate::{
    emby::ItemCounts,
    server::{CachedItemCounts, CachedServer},
};

use super::TinyApp;

impl TinyApp {
    pub(super) fn refresh_saved_server_counts(&mut self, server_id: &str, cx: &mut Context<Self>) {
        self.item_counts_loading.remove(server_id);
        self.item_counts_failed.remove(server_id);
        self.item_counts_refreshed.remove(server_id);
        self.load_item_counts_for_server_id(server_id, cx);
    }

    pub(super) fn load_item_counts_for_server_id(
        &mut self,
        server_id: &str,
        cx: &mut Context<Self>,
    ) {
        if self.item_counts_loading.contains(server_id)
            || self.item_counts_failed.contains(server_id)
            || self.item_counts_refreshed.contains(server_id)
        {
            return;
        }

        let Some(server) = self
            .servers
            .iter()
            .find(|server| server.id == server_id)
            .cloned()
        else {
            return;
        };
        if !server.can_reuse_auth() {
            return;
        }

        let Some(client) = self.emby_client.clone() else {
            self.push_app_error_notification("Emby HTTP 客户端不可用", cx);
            cx.notify();
            return;
        };

        let server_id = server.id.clone();
        self.item_counts_loading.insert(server_id.clone());

        let request_server = server.clone();
        let task = cx.background_spawn(async move { client.item_counts(&request_server) });

        cx.spawn(async move |app, cx| {
            let result = task.await;
            app.update(cx, |app, cx| {
                app.finish_item_counts(server, result, cx);
            })
            .ok();
        })
        .detach();
    }

    fn finish_item_counts(
        &mut self,
        server: CachedServer,
        result: Result<ItemCounts>,
        cx: &mut Context<Self>,
    ) {
        // Ignore requests started before an edit changed the endpoint or login.
        if !self.servers.iter().any(|current| {
            current.id == server.id
                && current.can_reuse_auth()
                && current.endpoint == server.endpoint
                && current.username == server.username
                && current.password == server.password
                && current.user_id == server.user_id
                && current.access_token == server.access_token
        }) {
            return;
        }
        let server_id = server.id;
        self.item_counts_loading.remove(&server_id);

        match result {
            Ok(counts) => {
                self.item_counts_failed.remove(&server_id);
                self.item_counts_refreshed.insert(server_id.clone());
                self.update_item_counts_cache(server_id, counts, cx);
            }
            Err(_) => {
                self.item_counts_failed.insert(server_id);
            }
        }

        cx.notify();
    }

    fn update_item_counts_cache(
        &mut self,
        server_id: String,
        counts: ItemCounts,
        cx: &mut Context<Self>,
    ) {
        let cached_counts = CachedItemCounts {
            movie_count: counts.movie_count,
            series_count: counts.series_count,
        };

        if let Some(server) = self
            .servers
            .iter_mut()
            .find(|server| server.id == server_id)
        {
            server.item_counts = Some(cached_counts.clone());
        }

        if let Some(server) = self
            .cache
            .servers
            .iter_mut()
            .find(|server| server.id == server_id)
        {
            server.item_counts = Some(cached_counts);
            self.schedule_cache_save("保存媒体数量缓存失败", cx);
        }

        self.item_counts.insert(server_id, counts);
    }

    pub(super) fn retain_item_count_state(&mut self) {
        self.item_counts
            .retain(|server_id, _| self.servers.iter().any(|server| &server.id == server_id));
        let authenticated_ids = self
            .servers
            .iter()
            .filter(|server| server.can_reuse_auth())
            .map(|server| server.id.clone())
            .collect::<HashSet<_>>();

        self.item_counts_loading
            .retain(|server_id| authenticated_ids.contains(server_id));
        self.item_counts_failed
            .retain(|server_id| authenticated_ids.contains(server_id));
        self.item_counts_refreshed
            .retain(|server_id| authenticated_ids.contains(server_id));
    }
}

pub(super) fn cached_item_counts_by_server(
    servers: &[CachedServer],
) -> HashMap<String, ItemCounts> {
    servers
        .iter()
        .filter_map(|server| {
            server
                .item_counts
                .as_ref()
                .map(|counts| (server.id.clone(), ItemCounts::from(counts)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
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
            app.item_counts_loading.insert(server.id.clone());
            let mut previous = server.clone();
            previous.access_token = Some("old-token".into());
            app.finish_item_counts(
                previous,
                Ok(ItemCounts {
                    movie_count: 999,
                    series_count: 999,
                    ..Default::default()
                }),
                cx,
            );
            assert!(app.item_counts_loading.contains(&server.id));
            assert!(!app.item_counts.contains_key(&server.id));
            assert!(app.cache.servers[0].item_counts.is_none());

            app.finish_item_counts(
                server.clone(),
                Ok(ItemCounts {
                    movie_count: 12,
                    series_count: 34,
                    ..Default::default()
                }),
                cx,
            );
            assert!(!app.item_counts_loading.contains(&server.id));
            assert_eq!(app.item_counts[&server.id].movie_count, 12);
            assert_eq!(
                app.cache.servers[0]
                    .item_counts
                    .as_ref()
                    .unwrap()
                    .series_count,
                34
            );

            // Saving an edit keeps the old display, even when credentials
            // are unchanged, but prevents using that login for new requests.
            app.servers[0].needs_auth_refresh = true;
            app.cache.servers[0].needs_auth_refresh = true;
            app.retain_item_count_state();
            app.load_item_counts_for_server_id(&server.id, cx);
            assert!(!app.item_counts_loading.contains(&server.id));
            assert!(!app.item_counts_refreshed.contains(&server.id));
            app.finish_item_counts(
                server.clone(),
                Ok(ItemCounts {
                    movie_count: 999,
                    series_count: 999,
                    ..Default::default()
                }),
                cx,
            );
            assert_eq!(app.item_counts[&server.id].movie_count, 12);
            assert_eq!(
                app.cache.servers[0]
                    .item_counts
                    .as_ref()
                    .unwrap()
                    .series_count,
                34
            );
        });
    }
}
