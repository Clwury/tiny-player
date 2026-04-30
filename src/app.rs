mod auth;
mod cache_save;
mod dialogs;
mod item_counts;
mod render;
mod resize;
mod server_cache;
mod server_card;
mod window;

use std::collections::{HashMap, HashSet};

use gpui::{Entity, SharedString};

use crate::{
    emby::{EmbyClient, ItemCounts},
    home::HomePage,
    server::CachedServer,
    storage::ServerCache,
    ui::add_server_dialog::AddServerDialogState,
};

pub struct TinyApp {
    add_server_dialog: Option<Entity<AddServerDialogState>>,
    open_server_menu: Option<String>,
    cache: ServerCache,
    emby_client: Option<EmbyClient>,
    servers: Vec<CachedServer>,
    cache_error: Option<SharedString>,
    item_counts: HashMap<String, ItemCounts>,
    item_counts_loading: HashSet<String>,
    item_counts_failed: HashSet<String>,
    item_counts_refreshed: HashSet<String>,
    window_bounds_observed: bool,
    window_persistence_enabled: bool,
    cache_save_generation: u64,
    pending_cache_save_error_prefix: Option<&'static str>,
    page: Page,
}

#[derive(Clone, Debug)]
enum Page {
    Servers,
    Home(Entity<HomePage>),
}

impl TinyApp {
    pub fn new(cache: ServerCache, cache_error: Option<SharedString>) -> Self {
        let servers = cache.servers.clone();
        let window_persistence_enabled = cache_error.is_none();
        let item_counts = item_counts::cached_item_counts_by_server(&servers);
        let (emby_client, emby_client_error) = match EmbyClient::new(cache.device_id.clone()) {
            Ok(client) => (Some(client), None),
            Err(error) => (None, Some(format!("{error}").into())),
        };
        let cache_error = cache_error.or(emby_client_error);

        Self {
            add_server_dialog: None,
            open_server_menu: None,
            cache,
            emby_client,
            servers,
            cache_error,
            item_counts,
            item_counts_loading: HashSet::new(),
            item_counts_failed: HashSet::new(),
            item_counts_refreshed: HashSet::new(),
            window_bounds_observed: false,
            window_persistence_enabled,
            cache_save_generation: 0,
            pending_cache_save_error_prefix: None,
            page: Page::Servers,
        }
    }
}
