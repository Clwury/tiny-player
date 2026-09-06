mod auth;
mod cache_save;
mod dialogs;
mod item_counts;
mod notification;
mod render;
mod resize;
mod server_cache;
mod server_card;
mod window;

use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

use gpui::{Context, Entity, SharedString, Task};

pub(crate) use resize::WINDOW_RESIZE_EDGE_WIDTH_PX;

use crate::{
    emby::{EmbyClient, ItemCounts},
    home::HomePage,
    player::PlaybackPage,
    server::CachedServer,
    storage::ServerCache,
    ui::add_server_dialog::AddServerDialogState,
    ui::playback_settings_dialog::PlaybackSettingsDialogState,
};
use notification::AppNotificationQueue;

pub struct TinyApp {
    add_server_dialog: Option<Entity<AddServerDialogState>>,
    playback_settings_dialog: Option<Entity<PlaybackSettingsDialogState>>,
    open_server_menu: Option<String>,
    cache: ServerCache,
    emby_client: Option<EmbyClient>,
    servers: Vec<CachedServer>,
    app_notifications: AppNotificationQueue,
    item_counts: HashMap<String, ItemCounts>,
    item_counts_loading: HashSet<String>,
    item_counts_failed: HashSet<String>,
    item_counts_refreshed: HashSet<String>,
    selecting_server_id: Option<String>,
    window_bounds_observed: bool,
    window_persistence_enabled: bool,
    pending_cache_save_error_prefix: Option<&'static str>,
    last_cache_save_activity: Option<Instant>,
    cache_save_task_active: bool,
    cache_save_task: Task<()>,
    #[cfg(test)]
    cache_save_path: Option<std::path::PathBuf>,
    page: Page,
}

#[derive(Clone, Debug)]
enum Page {
    Servers,
    Home(Entity<HomePage>),
    Playback {
        page: Entity<PlaybackPage>,
        return_to: Entity<HomePage>,
    },
}

impl TinyApp {
    pub fn new(
        cache: ServerCache,
        startup_error: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        cache.track_languages.apply(cx);
        let servers = cache.servers.clone();
        let window_persistence_enabled = startup_error.is_none();
        let item_counts = item_counts::cached_item_counts_by_server(&servers);
        let (emby_client, emby_client_error) = match EmbyClient::new(cache.device_id.clone()) {
            Ok(client) => (Some(client), None),
            Err(error) => (None, Some(format!("{error}").into())),
        };
        let initial_error = startup_error.or(emby_client_error);
        cx.on_release(|app, _| app.save_pending_cache_on_release())
            .detach();
        cx.on_app_quit(|app, cx| {
            app.flush_scheduled_cache_save(cx);
            async {}
        })
        .detach();
        let mut app = Self {
            add_server_dialog: None,
            playback_settings_dialog: None,
            open_server_menu: None,
            cache,
            emby_client,
            servers,
            app_notifications: AppNotificationQueue::default(),
            item_counts,
            item_counts_loading: HashSet::new(),
            item_counts_failed: HashSet::new(),
            item_counts_refreshed: HashSet::new(),
            selecting_server_id: None,
            window_bounds_observed: false,
            window_persistence_enabled,
            pending_cache_save_error_prefix: None,
            last_cache_save_activity: None,
            cache_save_task_active: false,
            cache_save_task: Task::ready(()),
            #[cfg(test)]
            cache_save_path: None,
            page: Page::Servers,
        };
        if let Some(error) = initial_error {
            app.push_app_error_notification(error, cx);
        }
        app
    }
}
