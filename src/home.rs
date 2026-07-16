mod cache;
mod carousel;
mod components;
mod data;
mod detail;
mod favorites;
mod library;
mod navigation;
mod paged_items;
mod playback;
mod render;
mod search;
mod sidebar;
mod workspace_render;

use std::collections::{HashMap, HashSet, VecDeque};

use crate::{
    emby::{EmbyClient, ResumeItems, UserItemData, UserItems, UserViews},
    images::loader::ImageLoader,
    player::PlaybackRequest,
    server::CachedServer,
    ui::text_input::TextInput,
};
use carousel::CarouselState;
use favorites::FavoriteRollback;
use library::LibraryState;
use navigation::{HomeNavigation, HomeRoot, HomeRoute};
use paged_items::PagedItemsState;
use search::SearchState;

pub(crate) use detail::SeriesDetailState;

use gpui::{
    App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, ScrollHandle, SharedString,
    WeakEntity, Window,
};

#[derive(Clone, Debug)]
pub enum HomeEvent {
    BackToServers,
    SectionChanged,
    TitleChanged,
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
    failed: Option<gpui::SharedString>,
    carousel: CarouselState,
}

#[derive(Clone, Debug)]
pub struct HomePage {
    current_server: CachedServer,
    servers: Vec<CachedServer>,
    home_content: Entity<HomeContent>,
}

#[derive(Clone, Debug)]
struct HomeContent {
    current_server: CachedServer,
    emby_client: EmbyClient,
    home_dashboard: Entity<HomeDashboard>,
    defer_home_dashboard_refresh: bool,
    authentication_error: Option<SharedString>,
    navigation: HomeNavigation,
    home_refresh_generation: u64,
    home_effects: HomeEffects,
    user_views: Option<UserViews>,
    user_views_failed: Option<gpui::SharedString>,
    user_views_carousel: CarouselState,
    resume_items: Option<ResumeItems>,
    resume_items_failed: Option<gpui::SharedString>,
    resume_detail_failed: Option<gpui::SharedString>,
    resume_items_carousel: CarouselState,
    user_view_items_rows: HashMap<String, UserViewItemsRow>,
    latest_queue: VecDeque<String>,
    latest_in_flight: HashSet<(String, u64)>,
    libraries: HashMap<String, LibraryState>,
    favorites: PagedItemsState,
    search: SearchState,
    search_input: Entity<TextInput>,
    user_data_overrides: HashMap<String, UserItemData>,
    user_data_revision: u64,
    user_data_item_revisions: HashMap<String, u64>,
    favorite_requests: HashSet<String>,
    favorite_rollbacks: HashMap<String, FavoriteRollback>,
    favorite_failures: HashMap<String, SharedString>,
    series_detail: Option<SeriesDetailState>,
    detail_history: Vec<SeriesDetailState>,
    detail_generation: u64,
    home_scroll_handle: ScrollHandle,
    image_loader: ImageLoader,
    snapshot_save_generation: u64,
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
        let search_input = cx.new(|cx| TextInput::new("搜索电影或剧集", cx).clearable());
        cx.subscribe(&search_input, |page, _, event, cx| {
            page.on_search_input_event(event, cx);
        })
        .detach();
        let home_content = cx.weak_entity();
        let home_dashboard = cx.new(move |_| HomeDashboard { home_content });
        let observed_home_dashboard = home_dashboard.clone();
        cx.observe_self(move |page, cx| {
            if page.navigation.current() == &HomeRoute::Root(HomeRoot::Home) {
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
            defer_home_dashboard_refresh: false,
            authentication_error,
            navigation: HomeNavigation::default(),
            home_refresh_generation: 0,
            home_effects: HomeEffects::default(),
            user_views: None,
            user_views_failed: None,
            user_views_carousel: CarouselState::default(),
            resume_items: None,
            resume_items_failed: None,
            resume_detail_failed: None,
            resume_items_carousel: CarouselState::default(),
            user_view_items_rows: HashMap::new(),
            latest_queue: VecDeque::new(),
            latest_in_flight: HashSet::new(),
            libraries: HashMap::new(),
            favorites: PagedItemsState::default(),
            search: SearchState::default(),
            search_input,
            user_data_overrides: HashMap::new(),
            user_data_revision: 0,
            user_data_item_revisions: HashMap::new(),
            favorite_requests: HashSet::new(),
            favorite_rollbacks: HashMap::new(),
            favorite_failures: HashMap::new(),
            series_detail: None,
            detail_history: Vec::new(),
            detail_generation: 0,
            home_scroll_handle: ScrollHandle::new(),
            image_loader: ImageLoader::new(),
            snapshot_save_generation: 0,
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

        let entering_home = root == HomeRoot::Home;
        self.sync_previous_offsets();
        self.defer_home_dashboard_refresh |= entering_home;
        self.detail_generation = self.detail_generation.wrapping_add(1);
        self.series_detail = None;
        self.detail_history.clear();
        self.favorite_failures.clear();
        match root {
            HomeRoot::Home => self.start_effects(cx),
            HomeRoot::Favorites if self.authentication_error.is_none() => {
                self.enter_favorites_if_needed(cx)
            }
            HomeRoot::Search if self.authentication_error.is_none() => {
                if !self.search.focused_once {
                    let focus = self.search_input.read(cx).focus_handle(cx);
                    window.focus(&focus);
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
