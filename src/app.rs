mod auth;
mod cache_save;
mod dialogs;
mod intent;
mod item_counts;
mod notification;
mod render;
mod resize;
mod server_cache;
#[cfg(test)]
mod server_card;
mod server_icon_picker;
mod server_reorder;
mod server_view;
mod settings_window;
mod shell;
mod window;

use std::collections::HashMap;

use gpui::{Context, Entity, SharedString, Task, WindowHandle};

#[cfg(target_os = "windows")]
pub(crate) use window::windows::prevent_playback_idle;
pub(crate) use window::{
    app_window_options, window_content_size, window_corner_radii, window_uses_system_decorations,
};

use crate::server::view::ServerContextMenu;
use crate::{
    emby::EmbyClient, home::HomePage, player::PlaybackPage,
    server::view::form::AddServerDialogState, storage::ServerCache,
};
use notification::AppNotificationQueue;

pub struct TinyApp {
    add_server_dialog: Option<Entity<AddServerDialogState>>,
    settings_window: Option<WindowHandle<settings_window::SettingsWindow>>,
    open_server_menu: Option<ServerContextMenu>,
    server_icon_picker: Option<server_icon_picker::ServerIconPicker>,
    server_reorder: Option<server_reorder::ServerReorder>,
    server_card_positions: HashMap<String, crate::server::view::reorder::CardPosition>,
    cache: crate::config::GlobalConfig,
    emby_client: Option<EmbyClient>,
    server_feature: crate::server::feature::ServerController,
    server_effects: ServerEffects,
    app_notifications: AppNotificationQueue,
    window_bounds_observed: bool,
    window_persistence_enabled: bool,
    persistence: crate::persistence::PersistenceService,
    #[cfg(test)]
    cache_save_path: Option<std::path::PathBuf>,
    shell: AppShell,
}

/// GPUI runner handles are separate from pure server state. Auth replacement,
/// count reset/deletion and shell release cancel delivery; tokens fence IO.
#[derive(Default)]
struct ServerEffects {
    auth: crate::effects::EffectHandle<Task<()>>,
    save: crate::effects::EffectHandle<Task<()>>,
    icon: crate::effects::EffectHandle<Task<()>>,
    counts: HashMap<String, crate::effects::EffectHandle<Task<()>>>,
}

type Page = shell::MountedPage<Entity<HomePage>, Entity<PlaybackPage>>;
type AppShell = shell::ShellController<Entity<HomePage>, Entity<PlaybackPage>, gpui::Subscription>;

impl TinyApp {
    pub fn new(
        cache: ServerCache,
        startup_error: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let persistence = crate::persistence::PersistenceService::get(cx);
        cache.track_languages.apply(cx);
        cache.items_sort.apply(cx);
        cx.observe_global::<crate::media::ItemSortPreferences>(|app, cx| {
            app.update_items_sort(crate::media::ItemSortPreferences::get(cx), cx);
        })
        .detach();
        let (cache, catalog) = crate::config::GlobalConfig::split(cache);
        let server_feature = crate::server::feature::ServerController::new(catalog);
        let window_persistence_enabled = startup_error.is_none();
        let (emby_client, emby_client_error) = match EmbyClient::new(cache.device_id.clone()) {
            Ok(client) => (Some(client), None),
            Err(error) => (None, Some(format!("{error}").into())),
        };
        let initial_error = startup_error.or(emby_client_error);
        cx.on_release(|app, cx| {
            if let Some(picker) = app.server_icon_picker.take() {
                picker.clear_previews(cx);
            }
            app.save_pending_cache_on_release(cx);
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
                    app.flush_persistence(cx).detach();
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.on_app_quit(|app, cx| app.flush_persistence(cx)).detach();
        let mut app = Self {
            add_server_dialog: None,
            settings_window: None,
            open_server_menu: None,
            server_icon_picker: None,
            server_reorder: None,
            server_card_positions: HashMap::new(),
            cache,
            emby_client,
            server_feature,
            server_effects: ServerEffects::default(),
            app_notifications: AppNotificationQueue::default(),
            window_bounds_observed: false,
            window_persistence_enabled,
            persistence,
            #[cfg(test)]
            cache_save_path: None,
            shell: AppShell::default(),
        };
        if let Some(error) = initial_error {
            app.push_app_error_notification(error, cx);
        } else if let Some(server) = app.server_feature.auto_start_server().cloned() {
            app.begin_select_server(&server, cx);
        }
        app
    }
}
