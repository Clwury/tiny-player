mod auth;
mod cache_save;
mod dialogs;
mod item_counts;
mod notification;
mod render;
mod resize;
mod server_cache;
mod server_card;
mod server_reorder;
mod settings_window;
mod window;

use std::{
    collections::{HashMap, HashSet},
    time::Instant,
};

use gpui::{Context, Entity, SharedString, Task, WindowHandle};

pub(crate) use resize::WINDOW_RESIZE_EDGE_WIDTH_PX;
pub(crate) use window::app_window_options;

use crate::{
    emby::{EmbyClient, ItemCounts},
    home::HomePage,
    player::PlaybackPage,
    server::CachedServer,
    storage::ServerCache,
    ui::add_server_dialog::AddServerDialogState,
};
use notification::AppNotificationQueue;
use server_card::ServerContextMenu;

pub struct TinyApp {
    add_server_dialog: Option<Entity<AddServerDialogState>>,
    settings_window: Option<WindowHandle<settings_window::SettingsWindow>>,
    open_server_menu: Option<ServerContextMenu>,
    server_reorder: Option<server_reorder::ServerReorder>,
    server_card_positions: HashMap<String, server_reorder::CardPosition>,
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
        cx.on_release(|app, cx| {
            app.save_pending_cache_on_release();
            if let Some(settings) = app.settings_window.take() {
                settings
                    .update(cx, |_, window, _| window.remove_window())
                    .ok();
            }
        })
        .detach();
        let app = cx.weak_entity();
        cx.on_window_closed(move |cx, window_id| {
            app.update(cx, |app, cx| {
                if app
                    .settings_window
                    .is_some_and(|window| window.window_id() == window_id)
                {
                    app.settings_window = None;
                    app.flush_scheduled_cache_save(cx);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.on_app_quit(|app, cx| {
            app.flush_scheduled_cache_save(cx);
            async {}
        })
        .detach();
        let mut app = Self {
            add_server_dialog: None,
            settings_window: None,
            open_server_menu: None,
            server_reorder: None,
            server_card_positions: HashMap::new(),
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
        } else if let Some(server) = app
            .cache
            .auto_start_server_id
            .as_ref()
            .and_then(|id| app.servers.iter().find(|server| &server.id == id))
            .cloned()
        {
            app.begin_select_server(&server, cx);
        }
        app
    }
}
