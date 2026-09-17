mod cache;
mod carousel;
mod components;
mod data;
mod detail;
mod favorites;
mod item_context_menu;
mod library;
mod navigation;
mod notification;
mod paged_items;
mod playback;
mod render;
mod resume_actions;
mod search;
mod sidebar;
mod track_preferences;
mod video_version;
mod visible_row;
mod workspace_render;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Instant,
};

use crate::{
    emby::{EmbyClient, ResumeItems, UserItemData, UserItems, UserViews},
    images::loader::ImageLoader,
    player::{PlaybackRequest, PlaybackTrackPreferences},
    server::CachedServer,
    ui::editor::Editor,
};
use carousel::CarouselState;
use favorites::{FavoriteRollback, FavoritesState};
use item_context_menu::ItemContextMenu;
use library::LibraryState;
use navigation::{HomeNavigation, HomeRoot, HomeRoute};
use notification::HomeNotificationQueue;
use search::SearchState;

pub(crate) use detail::SeriesDetailState;

use gpui::{
    App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, ScrollHandle, SharedString,
    Task, WeakEntity, Window,
};

#[derive(Clone, Debug)]
pub enum HomeEvent {
    BackToServers,
    SwitchServer(String),
    SectionChanged,
    TitleChanged,
    OpenSettings,
    OpenPlayback(Box<PlaybackRequest>),
}

#[derive(Clone, Debug)]
enum HomeContentEvent {
    TitleChanged,
    OpenPlayback(Box<PlaybackRequest>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LoadState {
    #[default]
    Idle,
    Loading,
    Loaded,
    Failed,
}

impl LoadState {
    fn can_start(self) -> bool {
        matches!(self, Self::Idle | Self::Failed)
    }

    fn is_loading(self) -> bool {
        matches!(self, Self::Loading)
    }
}

#[derive(Clone, Debug, Default)]
struct HomeEffects {
    home_snapshot: LoadState,
    user_views: LoadState,
    resume_items: LoadState,
}

#[derive(Clone, Debug, Default)]
struct UserViewItemsRow {
    items: Option<UserItems>,
    loading: bool,
    carousel: CarouselState,
}

#[derive(Clone, Debug)]
pub struct HomePage {
    current_server: CachedServer,
    servers: Vec<CachedServer>,
    home_content: Entity<HomeContent>,
}

#[derive(Debug)]
struct HomeContent {
    current_server: CachedServer,
    emby_client: EmbyClient,
    home_dashboard: Entity<HomeDashboard>,
    reuse_home_dashboard_until_next_render: bool,
    last_window_size: Option<(u32, u32)>,
    resize_in_progress: bool,
    resize_generation: u64,
    last_resize_activity: Option<Instant>,
    resize_settle_task_active: bool,
    resize_settle_task: Task<()>,
    defer_workspace_grid_contraction: bool,
    defer_home_dashboard_warmup: bool,
    workspace_grid_columns: usize,
    authentication_error: Option<SharedString>,
    navigation: HomeNavigation,
    home_refresh_generation: u64,
    home_effects: HomeEffects,
    user_views: Option<UserViews>,
    user_views_failed: Option<gpui::SharedString>,
    user_views_carousel: CarouselState,
    resume_items: Option<ResumeItems>,
    resume_items_failed: Option<gpui::SharedString>,
    resume_items_carousel: CarouselState,
    item_context_menu: Option<ItemContextMenu>,
    resume_item_requests: HashSet<String>,
    user_view_items_rows: HashMap<String, UserViewItemsRow>,
    latest_queue: VecDeque<String>,
    latest_in_flight: HashSet<(String, u64)>,
    libraries: HashMap<String, LibraryState>,
    favorites: FavoritesState,
    search: SearchState,
    search_input: Entity<Editor>,
    user_data_overrides: HashMap<String, UserItemData>,
    played_video_versions: HashMap<String, video_version::VideoVersion>,
    user_data_revision: u64,
    user_data_item_revisions: HashMap<String, u64>,
    favorite_requests: HashSet<String>,
    favorite_rollbacks: HashMap<String, FavoriteRollback>,
    played_request: Option<detail::PlayedRequest>,
    series_user_data_revisions: HashMap<String, u64>,
    series_detail: Option<SeriesDetailState>,
    detail_history: Vec<SeriesDetailState>,
    detail_generation: u64,
    notifications: HomeNotificationQueue,
    home_scroll_handle: ScrollHandle,
    image_loader: ImageLoader,
    snapshot_save_generation: u64,
    snapshot_save_pending: bool,
    snapshot_save_task: Task<()>,
    #[cfg(test)]
    snapshot_save_path: Option<std::path::PathBuf>,
    playback_refresh_generation: u64,
}

#[derive(Debug)]
struct HomeDashboard {
    home_content: WeakEntity<HomeContent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WorkspaceIdentity {
    local_server_id: String,
    remote_server_id: Option<String>,
    user_id: Option<String>,
}

impl EventEmitter<HomeEvent> for HomePage {}

impl EventEmitter<HomeContentEvent> for HomeContent {}

impl HomeContent {
    fn new(current_server: CachedServer, emby_client: EmbyClient, cx: &mut Context<Self>) -> Self {
        cx.observe_global::<PlaybackTrackPreferences>(|page, cx| {
            if page.sync_track_preferences(cx) {
                page.schedule_home_snapshot_save(cx);
                cx.notify();
            }
        })
        .detach();
        cx.on_app_quit(|page, cx| page.finish_home_snapshot_saves(cx))
            .detach();
        cx.on_release(|page, cx| page.finish_home_snapshot_saves(cx).detach())
            .detach();
        let search_input = cx.new(|cx| Editor::new("搜索电影或剧集", cx).clearable());
        cx.subscribe(&search_input, |page, _, event, cx| {
            page.on_search_input_event(event, cx);
        })
        .detach();
        let home_content = cx.weak_entity();
        let home_dashboard = cx.new(move |_| HomeDashboard { home_content });
        let observed_home_dashboard = home_dashboard.clone();
        cx.observe_self(move |page, cx| {
            // The cached dashboard reads HomeContent state through a separate
            // entity, so invalidate it whenever visible Home state changes. A
            // route-only transition back to Home can reuse the warm dashboard
            // cache; later data notifications invalidate it normally.
            if page.navigation.current() == &HomeRoute::Root(HomeRoot::Home)
                && !page.reuse_home_dashboard_until_next_render
            {
                observed_home_dashboard.update(cx, |_, cx| cx.notify());
            }
        })
        .detach();
        let authentication_error = current_server
            .user_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .is_none()
            .then(|| "Emby 用户信息缺失，请返回服务器页重新登录".into());
        Self {
            current_server,
            emby_client,
            home_dashboard,
            reuse_home_dashboard_until_next_render: false,
            last_window_size: None,
            resize_in_progress: false,
            resize_generation: 0,
            last_resize_activity: None,
            resize_settle_task_active: false,
            resize_settle_task: Task::ready(()),
            defer_workspace_grid_contraction: false,
            defer_home_dashboard_warmup: false,
            workspace_grid_columns: 0,
            authentication_error,
            navigation: HomeNavigation::default(),
            home_refresh_generation: 0,
            home_effects: HomeEffects::default(),
            user_views: None,
            user_views_failed: None,
            user_views_carousel: CarouselState::default(),
            resume_items: None,
            resume_items_failed: None,
            resume_items_carousel: CarouselState::default(),
            item_context_menu: None,
            resume_item_requests: HashSet::new(),
            user_view_items_rows: HashMap::new(),
            latest_queue: VecDeque::new(),
            latest_in_flight: HashSet::new(),
            libraries: HashMap::new(),
            favorites: FavoritesState::default(),
            search: SearchState::default(),
            search_input,
            user_data_overrides: HashMap::new(),
            played_video_versions: HashMap::new(),
            user_data_revision: 0,
            user_data_item_revisions: HashMap::new(),
            favorite_requests: HashSet::new(),
            favorite_rollbacks: HashMap::new(),
            played_request: None,
            series_user_data_revisions: HashMap::new(),
            series_detail: None,
            detail_history: Vec::new(),
            detail_generation: 0,
            notifications: HomeNotificationQueue::default(),
            home_scroll_handle: ScrollHandle::new(),
            image_loader: ImageLoader::new(),
            snapshot_save_generation: 0,
            snapshot_save_pending: false,
            snapshot_save_task: Task::ready(()),
            #[cfg(test)]
            snapshot_save_path: None,
            playback_refresh_generation: 0,
        }
    }

    pub(super) fn start_effects(&mut self, cx: &mut Context<Self>) {
        if self.authentication_error.is_some() {
            cx.notify();
            return;
        }
        self.load_home_snapshot_if_needed(cx);
    }

    fn title(&self) -> SharedString {
        match self.navigation.current() {
            HomeRoute::Detail { .. } => self
                .series_detail
                .as_ref()
                .map(|detail| detail.title.clone().into())
                .unwrap_or_else(|| self.navigation.root().title().into()),
            route => route
                .title()
                .unwrap_or_else(|| self.navigation.root().title())
                .to_string()
                .into(),
        }
    }

    fn root(&self) -> HomeRoot {
        self.navigation.root()
    }

    fn select_root(&mut self, root: HomeRoot, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.navigation.select_root(root) {
            return false;
        }

        // Non-Home workspaces keep the last dashboard frame cached behind their
        // opaque overlay. Do not dirty that cache for the route-only Home update.
        self.reuse_home_dashboard_until_next_render = root == HomeRoot::Home;

        self.sync_previous_offsets();
        self.detail_generation = self.detail_generation.wrapping_add(1);
        // Detail responses can contain many episodes, people and media-source
        // strings. Keep them alive through the next frame so switching a sidebar
        // item can present the new workspace before doing all of that deallocation.
        let detail_to_drop = self.series_detail.take();
        let detail_history_to_drop = std::mem::take(&mut self.detail_history);
        if detail_to_drop.is_some() || !detail_history_to_drop.is_empty() {
            window.on_next_frame(move |window, _| {
                window.on_next_frame(move |_, _| drop((detail_to_drop, detail_history_to_drop)));
                // `on_next_frame` callbacks run outside an element's
                // layout/prepaint/paint context, so `request_animation_frame`
                // would try to read `current_view` and panic. `refresh` is the
                // phase-independent way to request the follow-up frame here.
                window.refresh();
            });
        }
        self.clear_all_notifications();
        self.item_context_menu = None;
        match root {
            HomeRoot::Home => self.start_effects(cx),
            HomeRoot::Favorites if self.authentication_error.is_none() => {
                self.enter_favorites_if_needed(cx)
            }
            HomeRoot::Search if self.authentication_error.is_none() => {
                if !self.search.focused_once {
                    let focus = self.search_input.read(cx).focus_handle(cx);
                    window.focus(&focus, cx);
                    self.search.focused_once = true;
                }
            }
            HomeRoot::Favorites => {}
            HomeRoot::Search => {}
        }
        cx.emit(HomeContentEvent::TitleChanged);
        true
    }

    pub(super) fn request_identity(&self) -> WorkspaceIdentity {
        WorkspaceIdentity {
            local_server_id: self.current_server.id.clone(),
            remote_server_id: self.current_server.server_id.clone(),
            user_id: self.current_server.user_id.clone(),
        }
    }

    pub(super) fn matches_request_identity(&self, identity: &WorkspaceIdentity) -> bool {
        self.request_identity() == *identity
    }

    fn sync_previous_offsets(&mut self) {
        self.favorites.sync_previous_offsets();
        self.user_views_carousel.sync_previous_offset();
        self.resume_items_carousel.sync_previous_offset();
        for row in self.user_view_items_rows.values_mut() {
            row.carousel.sync_previous_offset();
        }
        if let Some(detail) = &mut self.series_detail {
            detail.episodes_carousel.sync_previous_offset();
            detail.people_carousel.sync_previous_offset();
            detail.similar_carousel.sync_previous_offset();
        }
    }
}

impl HomePage {
    pub fn new(
        current_server: CachedServer,
        servers: Vec<CachedServer>,
        emby_client: EmbyClient,
        cx: &mut Context<Self>,
    ) -> Self {
        let home_content = cx.new(|cx| HomeContent::new(current_server.clone(), emby_client, cx));
        cx.subscribe(
            &home_content,
            |_: &mut HomePage, _, event, cx| match event {
                HomeContentEvent::TitleChanged => cx.emit(HomeEvent::TitleChanged),
                HomeContentEvent::OpenPlayback(request) => {
                    cx.emit(HomeEvent::OpenPlayback(request.clone()))
                }
            },
        )
        .detach();
        let mut page = Self {
            current_server,
            servers,
            home_content,
        };
        page.start_effects(cx);
        page
    }

    pub fn title(&self, cx: &App) -> SharedString {
        self.home_content.read(cx).title()
    }

    pub(crate) fn start_effects(&mut self, cx: &mut Context<Self>) {
        self.home_content
            .update(cx, |content, cx| content.start_effects(cx));
    }

    fn set_active_section(
        &mut self,
        section: HomeRoot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let changed = self
            .home_content
            .update(cx, |content, cx| content.select_root(section, window, cx));
        if changed {
            cx.emit(HomeEvent::SectionChanged);
            cx.notify();
        }
    }

    fn select_home_section(
        &mut self,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_section(HomeRoot::Home, window, cx);
        if event.click_count() == 2 {
            self.home_content
                .update(cx, |content, cx| content.refresh_home_content(cx));
            window.refresh();
        }
    }

    fn select_favorites_section(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_section(HomeRoot::Favorites, window, cx);
    }

    fn select_search_section(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_section(HomeRoot::Search, window, cx);
    }

    fn back_to_servers(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(HomeEvent::BackToServers);
    }

    fn switch_server(&mut self, server_id: &str, cx: &mut Context<Self>) {
        if server_id != self.current_server.id
            && self.servers.iter().any(|server| server.id == server_id)
        {
            cx.emit(HomeEvent::SwitchServer(server_id.to_string()));
        }
    }

    fn open_settings(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(HomeEvent::OpenSettings);
    }

    pub(crate) fn apply_playback_update(
        &mut self,
        update: crate::player::PlaybackStateUpdate,
        cx: &mut Context<Self>,
    ) {
        self.home_content.update(cx, |content, cx| {
            content.apply_playback_update(update, cx);
        });
    }
}
