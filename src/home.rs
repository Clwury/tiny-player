mod carousel;
mod components;
mod data;
mod render;
mod sidebar;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use crate::{
    emby::{ResumeItems, UserViews},
    image_cache::CachedImageKey,
    server::CachedServer,
};
use gpui::{ClickEvent, Context, EventEmitter, Window};

#[derive(Clone, Copy, Debug)]
pub enum HomeEvent {
    BackToServers,
    SectionChanged,
}

#[derive(Clone, Debug)]
pub struct HomePage {
    current_server: CachedServer,
    servers: Vec<CachedServer>,
    device_id: String,
    active_section: HomeSection,
    user_views: Option<UserViews>,
    user_views_loading: bool,
    user_views_failed: Option<gpui::SharedString>,
    user_views_scroll_offset: f32,
    user_views_previous_scroll_offset: f32,
    user_views_animation_id: u64,
    user_views_hovered: bool,
    user_views_controls_hovered: bool,
    resume_items: Option<ResumeItems>,
    resume_items_loading: bool,
    resume_items_failed: Option<gpui::SharedString>,
    resume_items_scroll_offset: f32,
    resume_items_previous_scroll_offset: f32,
    resume_items_animation_id: u64,
    resume_items_hovered: bool,
    resume_items_controls_hovered: bool,
    image_paths: HashMap<CachedImageKey, PathBuf>,
    images_loading: HashSet<CachedImageKey>,
    images_failed: HashSet<CachedImageKey>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HomeSection {
    Home,
    Favorites,
    Search,
}

impl EventEmitter<HomeEvent> for HomePage {}

impl HomeSection {
    fn title(self) -> &'static str {
        match self {
            Self::Home => "首页",
            Self::Favorites => "收藏",
            Self::Search => "搜索",
        }
    }
}

impl HomePage {
    pub fn new(
        current_server: CachedServer,
        servers: Vec<CachedServer>,
        device_id: String,
    ) -> Self {
        Self {
            current_server,
            servers,
            device_id,
            active_section: HomeSection::Home,
            user_views: None,
            user_views_loading: false,
            user_views_failed: None,
            user_views_scroll_offset: 0.0,
            user_views_previous_scroll_offset: 0.0,
            user_views_animation_id: 0,
            user_views_hovered: false,
            user_views_controls_hovered: false,
            resume_items: None,
            resume_items_loading: false,
            resume_items_failed: None,
            resume_items_scroll_offset: 0.0,
            resume_items_previous_scroll_offset: 0.0,
            resume_items_animation_id: 0,
            resume_items_hovered: false,
            resume_items_controls_hovered: false,
            image_paths: HashMap::new(),
            images_loading: HashSet::new(),
            images_failed: HashSet::new(),
        }
    }

    pub fn title(&self) -> &'static str {
        self.active_section.title()
    }

    fn set_active_section(&mut self, section: HomeSection, cx: &mut Context<Self>) {
        if self.active_section == section {
            return;
        }

        if section == HomeSection::Home {
            self.user_views_previous_scroll_offset = self.user_views_scroll_offset;
            self.resume_items_previous_scroll_offset = self.resume_items_scroll_offset;
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
