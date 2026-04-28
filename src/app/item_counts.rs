use std::collections::{HashMap, HashSet};

use anyhow::Result;
use gpui::{AppContext as _, Context};

use crate::{
    emby::{EmbyClient, ItemCounts},
    server::{CachedItemCounts, CachedServer},
    storage,
};

use super::TinyApp;

impl TinyApp {
    pub(super) fn load_item_counts_for_authenticated_servers(&mut self, cx: &mut Context<Self>) {
        let device_id = self.cache.device_id.clone();
        let servers = self.servers.clone();

        for server in servers {
            if !has_cached_auth(&server)
                || self.item_counts_loading.contains(&server.id)
                || self.item_counts_failed.contains(&server.id)
                || self.item_counts_refreshed.contains(&server.id)
            {
                continue;
            }

            let server_id = server.id.clone();
            self.item_counts_loading.insert(server_id.clone());

            let device_id = device_id.clone();
            let task = cx.background_spawn(async move {
                let client = EmbyClient::new(device_id)?;
                client.item_counts(&server)
            });

            cx.spawn(async move |app, cx| {
                let result = task.await;
                app.update(cx, |app, cx| {
                    app.finish_item_counts(server_id, result, cx);
                })
                .ok();
            })
            .detach();
        }
    }

    fn finish_item_counts(
        &mut self,
        server_id: String,
        result: Result<ItemCounts>,
        cx: &mut Context<Self>,
    ) {
        self.item_counts_loading.remove(&server_id);

        match result {
            Ok(counts) => {
                self.item_counts_failed.remove(&server_id);
                self.item_counts_refreshed.insert(server_id.clone());
                self.update_item_counts_cache(server_id, counts);
            }
            Err(_) => {
                self.item_counts_failed.insert(server_id);
            }
        }

        cx.notify();
    }

    fn update_item_counts_cache(&mut self, server_id: String, counts: ItemCounts) {
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
            if let Err(error) = storage::save(&self.cache) {
                self.cache_error = Some(format!("保存媒体数量缓存失败：{error}").into());
            }
        }

        self.item_counts.insert(server_id, counts);
    }

    pub(super) fn retain_item_count_state(&mut self) {
        let authenticated_ids = self
            .servers
            .iter()
            .filter(|server| has_cached_auth(server))
            .map(|server| server.id.clone())
            .collect::<HashSet<_>>();

        self.item_counts
            .retain(|server_id, _| authenticated_ids.contains(server_id));
        self.item_counts_loading
            .retain(|server_id| authenticated_ids.contains(server_id));
        self.item_counts_failed
            .retain(|server_id| authenticated_ids.contains(server_id));
        self.item_counts_refreshed
            .retain(|server_id| authenticated_ids.contains(server_id));
    }
}

fn has_cached_auth(server: &CachedServer) -> bool {
    server
        .user_id
        .as_deref()
        .is_some_and(|user_id| !user_id.is_empty())
        && server
            .access_token
            .as_deref()
            .is_some_and(|access_token| !access_token.is_empty())
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
