pub(crate) mod adapter;
pub(crate) mod cache;
mod carousel;
mod components;
mod content;
mod controller;
mod detail;
mod favorites;
mod feed;
mod gateway;
mod genre;
mod grid;
mod image_effects;
mod item_context_menu;
mod items_sort;
mod layout;
mod library;
mod model;
mod notification;
mod page;
mod person;
mod playback;
mod played;
mod ports;
mod presentation;
mod render;
mod resume_actions;
mod search;
mod sidebar;
mod snapshot;
mod track_preferences;
mod visible_row;

#[cfg(test)]
mod test_support;

pub(crate) use model::favorites::FavoriteItemType;
pub(crate) use ports::HomePorts;

use std::collections::HashMap;

use crate::{
    effects::{EffectHandle, WorkspaceIdentity},
    emby::EmbyClient,
    images::{
        ImageRepository,
        controller::{ImageController, ItemImageRequest},
        item_images::EmbyImageRepository,
    },
    media::video_version,
    player::PlaybackRequest,
    search_history::SearchHistory,
    server::CachedServer,
    settings::binding::PlaybackTrackPreferences,
    ui::editor::Editor,
};
use carousel::CarouselState;
use controller::HomeController;
use detail::binding::detail_binding;
use favorites::FavoritesPresentation;
use item_context_menu::ItemContextMenu;
use library::LibraryResources;
#[cfg(test)]
use model::paged_items;
use model::{LoadState, navigation};
use navigation::{HomeRoot, HomeRoute};
use notification::HomeNotificationQueue;
use search::SearchPresentation;

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, ScrollHandle, SharedString, Task,
    WeakEntity, Window,
};

#[derive(Clone, Debug)]
pub enum HomeEvent {
    BackToServers,
    SwitchServer(String),
    AddServer,
    ReorderServer {
        server_id: String,
        target_id: String,
    },
    SectionChanged,
    TitleChanged,
    SearchHistoryChanged(SearchHistory),
    OpenSettings,
    OpenPlayback(Box<PlaybackRequest>),
}

impl HomeEvent {
    pub(crate) fn trace(&self) {
        let operation = match self {
            Self::BackToServers => "home.back_to_servers",
            Self::SwitchServer(_) => "home.switch_server",
            Self::AddServer => "home.add_server",
            Self::ReorderServer { .. } => "home.reorder_server",
            Self::SectionChanged => "home.section_changed",
            Self::TitleChanged => "home.title_changed",
            Self::SearchHistoryChanged(_) => "home.search_history_changed",
            Self::OpenSettings => "home.open_settings",
            Self::OpenPlayback(_) => "home.open_playback",
        };
        crate::observability::TraceId::start(operation).record("received");
    }
}

#[derive(Clone, Debug)]
enum HomeContentEvent {
    TitleChanged,
    SearchHistoryChanged(SearchHistory),
    OpenPlayback(Box<PlaybackRequest>),
}

#[cfg(test)]
use feed::UserViewItemsRow;

#[derive(Clone, Debug)]
pub struct HomePage {
    sidebar: sidebar::controller::SidebarController,
    sidebar_scroll_handle: ScrollHandle,
    sidebar_reorder: Option<sidebar::reorder::SidebarReorder>,
    home_content: Entity<HomeContent>,
}

#[derive(Debug)]
struct HomeContent {
    current_server: CachedServer,
    emby_client: EmbyClient,
    layout: model::layout::HomeLayoutController,
    dashboard: presentation::HomeDashboardResources,
    authentication_error: Option<SharedString>,
    controller: HomeController,
    ports: HomePorts,
    feed_effects: feed::binding::FeedEffects,
    user_views_carousel: CarouselState,
    resume_items_carousel: CarouselState,
    latest_carousels: HashMap<String, CarouselState>,
    item_context_menu: Option<ItemContextMenu>,
    // One cancellable continuation per resume mutation target.
    resume_effects: HashMap<String, EffectHandle<Task<()>>>,
    library_resources: HashMap<String, LibraryResources>,
    genre_resources: HashMap<String, LibraryResources>,
    person_resources: HashMap<String, person::binding::PersonResources>,
    favorites_presentation: FavoritesPresentation,
    // Search scope: replace/cancel on query reset; dropping Home cancels delivery.
    search_effect: EffectHandle<Task<()>>,
    search_presentation: SearchPresentation,
    search_input: Entity<Editor>,
    // FavoriteMutation token validates delivery; release cancels this handle.
    favorite_effect: EffectHandle<Task<()>>,
    // PlayedMutation owns all followup reads; release cancels delivery.
    played_effect: EffectHandle<Task<()>>,
    detail_resources: HashMap<detail::controller::DetailId, detail::binding::DetailResources>,
    notifications: HomeNotificationQueue,
    home_scroll_handle: ScrollHandle,
    images: ImageController,
    // Image requests belong to this workspace; drop cancels every continuation.
    image_effects: HashMap<crate::images::cache::CachedImageKey, EffectHandle<Task<()>>>,
    image_repository:
        std::sync::Arc<dyn ImageRepository<ItemImageRequest, Image = std::path::PathBuf>>,
    // Shared app-level persistence; page release flushes only this workspace.
    persistence: crate::persistence::PersistenceService,
    #[cfg(test)]
    snapshot_save_path: Option<std::path::PathBuf>,
    // Wait for stop reporting before refreshing Home data. Replacement/release
    // cancels polling; only the current workspace token may trigger a refresh.
    playback_refresh_task: EffectHandle<Task<()>>,
}

#[derive(Debug)]
struct HomeDashboard {
    home_content: WeakEntity<HomeContent>,
    #[cfg(test)]
    render_count: usize,
}

impl EventEmitter<HomeEvent> for HomePage {}

impl EventEmitter<HomeContentEvent> for HomeContent {}
