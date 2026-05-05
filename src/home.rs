mod cache;
mod carousel;
mod components;
mod data;
mod detail;
mod render;
mod sidebar;

use std::collections::HashMap;

use crate::{
    emby::{EmbyClient, ResumeItems, UserItems, UserViews},
    images::loader::ImageLoader,
    server::CachedServer,
};
use carousel::CarouselState;

pub(crate) use detail::SeriesDetailState;

use gpui::{
    App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, ScrollHandle, SharedString,
    Window,
};

#[derive(Clone, Copy, Debug)]
pub enum HomeEvent {
    BackToServers,
    SectionChanged,
    TitleChanged,
}

#[derive(Clone, Copy, Debug)]
enum HomeContentEvent {
    TitleChanged,
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
        matches!(self, Self::Idle)
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

#[derive(Clone, Copy, Debug)]
struct MainScrollbarDragState {
    cursor_offset_y: f32,
    track_top: f32,
    track_height: f32,
    thumb_height: f32,
    max_offset: f32,
}

#[derive(Clone, Debug)]
pub struct HomePage {
    current_server: CachedServer,
    servers: Vec<CachedServer>,
    active_section: HomeSection,
    home_content: Entity<HomeContent>,
}

#[derive(Clone, Debug)]
struct HomeContent {
    current_server: CachedServer,
    emby_client: EmbyClient,
    home_effects: HomeEffects,
    user_views: Option<UserViews>,
    user_views_failed: Option<gpui::SharedString>,
    user_views_carousel: CarouselState,
    resume_items: Option<ResumeItems>,
    resume_items_failed: Option<gpui::SharedString>,
    resume_items_carousel: CarouselState,
    user_view_items_rows: HashMap<String, UserViewItemsRow>,
    series_detail: Option<SeriesDetailState>,
    main_scroll_handle: ScrollHandle,
    main_scrollbar_drag: Option<MainScrollbarDragState>,
    image_loader: ImageLoader,
    snapshot_save_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HomeSection {
    Home,
    Favorites,
    Search,
}

impl EventEmitter<HomeEvent> for HomePage {}

impl EventEmitter<HomeContentEvent> for HomeContent {}

impl HomeSection {
    fn title(self) -> &'static str {
        match self {
            Self::Home => "首页",
            Self::Favorites => "收藏",
            Self::Search => "搜索",
        }
    }
}

impl HomeContent {
    fn new(current_server: CachedServer, emby_client: EmbyClient) -> Self {
        Self {
            current_server,
            emby_client,
            home_effects: HomeEffects::default(),
            user_views: None,
            user_views_failed: None,
            user_views_carousel: CarouselState::default(),
            resume_items: None,
            resume_items_failed: None,
            resume_items_carousel: CarouselState::default(),
            user_view_items_rows: HashMap::new(),
            series_detail: None,
            main_scroll_handle: ScrollHandle::new(),
            main_scrollbar_drag: None,
            image_loader: ImageLoader::new(),
            snapshot_save_generation: 0,
        }
    }

    pub(super) fn start_effects(&mut self, cx: &mut Context<Self>) {
        self.load_home_snapshot_if_needed(cx);
    }

    fn title(&self, fallback: HomeSection) -> SharedString {
        self.series_detail
            .as_ref()
            .map(|detail| detail.title.clone().into())
            .unwrap_or_else(|| fallback.title().into())
    }

    fn sync_previous_offsets(&mut self) {
        self.user_views_carousel.sync_previous_offset();
        self.resume_items_carousel.sync_previous_offset();
        for row in self.user_view_items_rows.values_mut() {
            row.carousel.sync_previous_offset();
        }
        if let Some(detail) = &mut self.series_detail {
            detail.episodes_carousel.sync_previous_offset();
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
        let home_content = cx.new(|_| HomeContent::new(current_server.clone(), emby_client));
        cx.subscribe(
            &home_content,
            |_: &mut HomePage, _, event, cx| match event {
                HomeContentEvent::TitleChanged => cx.emit(HomeEvent::TitleChanged),
            },
        )
        .detach();
        let mut page = Self {
            current_server,
            servers,
            active_section: HomeSection::Home,
            home_content,
        };
        page.start_effects(cx);
        page
    }

    pub fn title(&self, cx: &App) -> SharedString {
        self.home_content.read(cx).title(self.active_section)
    }

    pub(crate) fn start_effects(&mut self, cx: &mut Context<Self>) {
        self.home_content
            .update(cx, |content, cx| content.start_effects(cx));
    }

    fn set_active_section(&mut self, section: HomeSection, cx: &mut Context<Self>) {
        if self.active_section == section {
            return;
        }

        if section == HomeSection::Home {
            self.home_content
                .update(cx, |content, _| content.sync_previous_offsets());
            self.start_effects(cx);
        }

        self.active_section = section;
        cx.emit(HomeEvent::SectionChanged);
    }

    fn select_home_section(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.set_active_section(HomeSection::Home, cx);
        cx.notify();
    }

    fn select_favorites_section(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.set_active_section(HomeSection::Favorites, cx);
        cx.notify();
    }

    fn select_search_section(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.set_active_section(HomeSection::Search, cx);
        cx.notify();
    }

    fn back_to_servers(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(HomeEvent::BackToServers);
    }
}
