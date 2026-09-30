use crate::home::detail::state::detail_binding;
mod adapter;
pub(crate) mod cache;
mod carousel;
mod components;
mod controller;
mod data;
mod detail;
mod favorites;
mod feed;
mod gateway;
mod item_context_menu;
mod layout;
mod library;
mod model;
mod notification;
mod playback;
mod played;
mod presentation;
mod render;
mod resume_actions;
mod search;
mod sidebar;
mod track_preferences;
mod video_version;
mod visible_row;
mod workspace_render;

use std::collections::HashMap;

use crate::{
    emby::EmbyClient,
    images::{
        ImageRepository,
        controller::{ImageController, ItemImageRequest},
        item_images::EmbyImageRepository,
    },
    player::{PlaybackRequest, PlaybackTrackPreferences},
    search_history::SearchHistory,
    server::CachedServer,
    ui::editor::Editor,
};
use carousel::CarouselState;
use controller::HomeController;
use favorites::FavoritesPresentation;
use item_context_menu::ItemContextMenu;
use library::LibraryResources;
#[cfg(test)]
use model::paged_items;
use model::{LoadState, navigation};
use navigation::{HomeRoot, HomeRoute};
use notification::HomeNotificationQueue;
use presentation::SearchPresentation;

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
    sidebar: model::sidebar::SidebarController,
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
    feed_effects: data::FeedEffects,
    user_views_carousel: CarouselState,
    resume_items_carousel: CarouselState,
    latest_carousels: HashMap<String, CarouselState>,
    item_context_menu: Option<ItemContextMenu>,
    // One cancellable continuation per resume mutation target.
    resume_effects: HashMap<String, EffectHandle<Task<()>>>,
    library_resources: HashMap<String, LibraryResources>,
    favorites_presentation: FavoritesPresentation,
    // Search scope: replace/cancel on query reset; dropping Home cancels delivery.
    search_effect: EffectHandle<Task<()>>,
    search_presentation: SearchPresentation,
    search_input: Entity<Editor>,
    // FavoriteMutation token validates delivery; release cancels this handle.
    favorite_effect: EffectHandle<Task<()>>,
    // PlayedMutation owns all followup reads; release cancels delivery.
    played_effect: EffectHandle<Task<()>>,
    detail_resources: HashMap<detail::controller::DetailId, detail::state::DetailResources>,
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

use crate::effects::{EffectHandle, WorkspaceIdentity};

impl EventEmitter<HomeEvent> for HomePage {}

impl EventEmitter<HomeContentEvent> for HomeContent {}

impl HomeContent {
    fn new(current_server: CachedServer, emby_client: EmbyClient, cx: &mut Context<Self>) -> Self {
        let persistence = crate::persistence::PersistenceService::get(cx);
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
        let search_input = cx.new(|cx| Editor::new("搜索电影或剧集", cx).search());
        cx.subscribe(&search_input, |page, _, event, cx| {
            page.on_search_input_event(event, cx);
        })
        .detach();
        let home_content = cx.weak_entity();
        let home_dashboard = cx.new(move |_| HomeDashboard {
            home_content,
            #[cfg(test)]
            render_count: 0,
        });
        let observed_home_dashboard = home_dashboard.clone();
        cx.observe_self(move |page, cx| {
            // The cached dashboard reads HomeContent state through a separate
            // entity, so invalidate it whenever visible Home state changes. A
            // route-only transition back to Home can reuse the warm dashboard
            // cache; later data notifications invalidate it normally.
            if page.layout.page_notified(page.controller.route()) {
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
        let identity = current_server.workspace_identity();
        let feed_effects = data::FeedEffects::new(identity.clone());
        let images = ImageController::new(identity.clone());
        let image_repository = std::sync::Arc::new(EmbyImageRepository {
            client: emby_client.clone(),
            server: current_server.clone(),
        });
        let layout = model::layout::HomeLayoutController::new(identity.clone());
        let notifications = HomeNotificationQueue::new(identity.clone());
        let controller = HomeController::new(identity);
        Self {
            current_server,
            emby_client,
            layout,
            dashboard: presentation::HomeDashboardResources::new(home_dashboard),
            authentication_error,
            controller,
            feed_effects,
            user_views_carousel: CarouselState::default(),
            resume_items_carousel: CarouselState::default(),
            latest_carousels: HashMap::new(),
            item_context_menu: None,
            resume_effects: HashMap::new(),
            library_resources: HashMap::new(),
            favorites_presentation: FavoritesPresentation::new(),
            search_effect: EffectHandle::default(),
            search_presentation: SearchPresentation::default(),
            search_input,
            favorite_effect: EffectHandle::default(),
            played_effect: EffectHandle::default(),
            detail_resources: HashMap::new(),
            notifications,
            home_scroll_handle: ScrollHandle::new(),
            images,
            image_effects: HashMap::new(),
            image_repository,
            persistence,
            #[cfg(test)]
            snapshot_save_path: None,
            playback_refresh_task: EffectHandle::default(),
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
        self.controller.title().to_owned().into()
    }

    fn detail_view(&self) -> Option<detail::state::DetailView<'_>> {
        detail::state::detail_view(self.controller.detail_view(), &self.detail_resources)
    }

    #[cfg(test)]
    fn install_detail_fixture(&mut self, fixture: Option<detail::test_fixture::DetailFixture>) {
        if let Some(current) = self.controller.test_state().navigation.detail() {
            self.detail_resources.remove(&current.id());
        }
        let controller = fixture.map(|fixture| {
            let (controller, resources) = fixture.into_parts();
            self.detail_resources.insert(controller.id(), resources);
            controller
        });
        self.controller
            .test_state_mut()
            .navigation
            .set_detail_fixture(controller);
    }

    fn root(&self) -> HomeRoot {
        self.controller.root()
    }

    fn select_root(&mut self, root: HomeRoot, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.controller.route() == &HomeRoute::Root(root) {
            return false;
        }
        self.sync_previous_offsets();
        let change = self
            .controller
            .dispatch_navigation(controller::NavigationIntent::Root(root));
        if !change.changed {
            return false;
        }

        // Non-Home workspaces keep the last dashboard frame cached behind their
        // opaque overlay. Do not dirty that cache for the route-only Home update.
        self.layout.root_selected(root);

        // Keep domain data and view handles alive for the original two-frame
        // release boundary, after cancelling all retired continuations now.
        let retired = detail::state::apply_navigation_change(change, &mut self.detail_resources);
        if !retired.0.is_empty() || !retired.1.is_empty() {
            window.on_next_frame(move |window, _| {
                window.on_next_frame(move |_, _| drop(retired));
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
                if !self.search_presentation.focused_once {
                    let focus = self.search_input.read(cx).focus_handle(cx);
                    window.focus(&focus, cx);
                    self.search_presentation.focused_once = true;
                }
            }
            HomeRoot::Favorites => {}
            HomeRoot::Search => {}
        }
        cx.emit(HomeContentEvent::TitleChanged);
        true
    }

    pub(super) fn request_identity(&self) -> WorkspaceIdentity {
        self.current_server.workspace_identity()
    }

    fn sync_previous_offsets(&mut self) {
        self.favorites_presentation.sync_previous_offsets();
        self.user_views_carousel.sync_previous_offset();
        self.resume_items_carousel.sync_previous_offset();
        for carousel in self.latest_carousels.values_mut() {
            carousel.sync_previous_offset();
        }
        if let Some(detail) =
            detail_binding(self.controller.detail_view(), &mut self.detail_resources)
        {
            detail.presentation.episodes_carousel.sync_previous_offset();
            detail.presentation.people_carousel.sync_previous_offset();
            detail.presentation.similar_carousel.sync_previous_offset();
        }
    }
}

impl HomePage {
    pub(crate) fn new(
        current_server: CachedServer,
        servers: Vec<crate::server::feature::SidebarServer>,
        emby_client: EmbyClient,
        cx: &mut Context<Self>,
    ) -> Self {
        let home_content = cx.new(|cx| HomeContent::new(current_server.clone(), emby_client, cx));
        cx.subscribe(
            &home_content,
            |_: &mut HomePage, _, event, cx| match event {
                HomeContentEvent::TitleChanged => cx.emit(HomeEvent::TitleChanged),
                HomeContentEvent::SearchHistoryChanged(history) => {
                    cx.emit(HomeEvent::SearchHistoryChanged(history.clone()))
                }
                HomeContentEvent::OpenPlayback(request) => {
                    cx.emit(HomeEvent::OpenPlayback(request.clone()))
                }
            },
        )
        .detach();
        let mut page = Self {
            sidebar: model::sidebar::SidebarController::new(
                current_server.id,
                current_server.username,
                servers,
            ),
            sidebar_scroll_handle: ScrollHandle::new(),
            sidebar_reorder: None,
            home_content,
        };
        page.start_effects(cx);
        page
    }

    pub fn title(&self, cx: &App) -> SharedString {
        self.home_content.read(cx).title()
    }

    pub(crate) fn set_servers(
        &mut self,
        servers: Vec<crate::server::feature::SidebarServer>,
        cx: &mut Context<Self>,
    ) {
        self.apply_sidebar_intent(model::sidebar::SidebarIntent::ServersChanged(servers), cx);
    }

    pub(crate) fn set_search_history(&mut self, history: SearchHistory, cx: &mut Context<Self>) {
        self.home_content.update(cx, |content, cx| {
            content.controller.restore_search_history(history);
            cx.notify();
        });
    }

    #[cfg(test)]
    pub(crate) fn current_server_id(&self) -> &str {
        self.sidebar.current_server_id()
    }

    pub(crate) fn set_selecting_server(
        &mut self,
        server_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.apply_sidebar_intent(
            model::sidebar::SidebarIntent::SelectingChanged(server_id),
            cx,
        );
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
