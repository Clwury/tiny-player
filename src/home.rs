use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use gpui::{
    App, AppContext, ClickEvent, Context, EventEmitter, InteractiveElement, IntoElement,
    MouseButton, ObjectFit, ParentElement, Render, StatefulInteractiveElement, Styled, StyledImage,
    Window, div, img, prelude::FluentBuilder, px, svg,
};

use crate::{
    emby::{EmbyClient, EmbyImageRequest, ImageQuality, UserViews},
    image_cache::{self, CachedImageKey},
    server::CachedServer,
    theme,
};

#[derive(Clone, Copy, Debug)]
pub enum HomeEvent {
    BackToServers,
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
    image_paths: HashMap<CachedImageKey, PathBuf>,
    images_loading: HashSet<CachedImageKey>,
    images_failed: HashSet<CachedImageKey>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HomeSection {
    Home,
    Favorites,
    Search,
}

impl EventEmitter<HomeEvent> for HomePage {}

impl HomeSection {
    pub fn title(self) -> &'static str {
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
            image_paths: HashMap::new(),
            images_loading: HashSet::new(),
            images_failed: HashSet::new(),
        }
    }

    pub fn set_active_section(&mut self, section: HomeSection) {
        self.active_section = section;
    }

    fn select_home_section(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.set_active_section(HomeSection::Home);
        cx.notify();
    }

    fn select_favorites_section(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.set_active_section(HomeSection::Favorites);
        cx.notify();
    }

    fn select_search_section(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.set_active_section(HomeSection::Search);
        cx.notify();
    }

    fn back_to_servers(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(HomeEvent::BackToServers);
    }

    fn render_main_content(&self, cx: &Context<Self>, rounded_window: bool) -> impl IntoElement {
        let theme = theme::get(cx);

        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .id("home-main-content")
            .overflow_y_scroll()
            .bg(theme.background)
            .p_6()
            .when(rounded_window, |this| {
                this.rounded_br(theme.radius_lg).overflow_hidden()
            })
            .child(
                div()
                    .mb_5()
                    .text_2xl()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child(self.active_section.title()),
            )
            .when(self.user_views_loading, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("正在加载媒体库…"),
                )
            })
            .when_some(self.user_views_failed.clone(), |this, error| {
                this.child(div().text_sm().text_color(theme.error).child(error))
            })
            .when_some(self.user_views.as_ref(), |this, views| {
                this.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_4()
                        .children(views.items.iter().map(|view| {
                            let image_path = self.image_path_for_item(&view.id);
                            user_view_card(view.name.clone(), image_path, cx)
                        })),
                )
            })
            .when(
                !self.user_views_loading
                    && self.user_views_failed.is_none()
                    && self
                        .user_views
                        .as_ref()
                        .is_none_or(|views| views.items.is_empty()),
                |this| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("暂无媒体库"),
                    )
                },
            )
    }

    fn load_user_views_if_needed(&mut self, cx: &mut Context<Self>) {
        if self.active_section != HomeSection::Home
            || self.user_views.is_some()
            || self.user_views_loading
            || self.user_views_failed.is_some()
        {
            return;
        }

        self.user_views_loading = true;
        let server = self.current_server.clone();
        let device_id = self.device_id.clone();
        let task = cx.background_spawn(async move {
            let client = EmbyClient::new(device_id)?;
            client.user_views(&server)
        });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| page.finish_user_views(result, cx))
                .ok();
        })
        .detach();
    }

    fn finish_user_views(&mut self, result: anyhow::Result<UserViews>, cx: &mut Context<Self>) {
        self.user_views_loading = false;

        match result {
            Ok(views) => {
                self.user_views_failed = None;
                self.ensure_user_view_images(&views, cx);
                self.user_views = Some(views);
            }
            Err(error) => {
                self.user_views_failed = Some(format!("加载首页失败：{error}").into());
            }
        }

        cx.notify();
    }

    fn ensure_user_view_images(&mut self, views: &UserViews, cx: &mut Context<Self>) {
        for view in &views.items {
            let Some(tag) = view
                .image_tags
                .as_ref()
                .and_then(|tags| tags.primary.clone())
            else {
                continue;
            };
            let request = EmbyImageRequest::primary(view.id.clone(), Some(tag))
                .with_max_width(640)
                .with_quality(ImageQuality::DEFAULT);
            let Some(key) = CachedImageKey::from_request(&self.current_server, &request) else {
                continue;
            };

            if self.image_paths.contains_key(&key)
                || self.images_loading.contains(&key)
                || self.images_failed.contains(&key)
            {
                continue;
            }

            match image_cache::cached_image_exists(&key) {
                Ok(Some(path)) => {
                    self.image_paths.insert(key, path);
                }
                Ok(None) => {
                    self.load_user_view_image(request, key, cx);
                }
                Err(_) => {
                    self.images_failed.insert(key);
                }
            }
        }
    }

    fn load_user_view_image(
        &mut self,
        request: EmbyImageRequest,
        key: CachedImageKey,
        cx: &mut Context<Self>,
    ) {
        self.images_loading.insert(key.clone());
        let server = self.current_server.clone();
        let task_key = key.clone();
        let device_id = self.device_id.clone();
        let task = cx.background_spawn(async move {
            let client = EmbyClient::new(device_id)?;
            let bytes = client.item_image(&server, &request)?;
            image_cache::write_cached_image(&task_key, &bytes)
        });

        cx.spawn(async move |page, cx| {
            let result = task.await;
            page.update(cx, |page, cx| page.finish_user_view_image(key, result, cx))
                .ok();
        })
        .detach();
    }

    fn finish_user_view_image(
        &mut self,
        key: CachedImageKey,
        result: anyhow::Result<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.images_loading.remove(&key);

        match result {
            Ok(path) => {
                self.images_failed.remove(&key);
                self.image_paths.insert(key, path);
            }
            Err(_) => {
                self.images_failed.insert(key);
            }
        }

        cx.notify();
    }

    fn image_path_for_item(&self, item_id: &str) -> Option<PathBuf> {
        self.image_paths
            .iter()
            .find(|(key, _)| key.item_id == item_id)
            .map(|(_, path)| path.clone())
    }

    fn render_sidebar(
        &self,
        cx: &Context<Self>,
        rounded_window: bool,
        on_back: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
        on_home: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_favorites: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_search: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let username = self.current_server.username.clone();

        div()
            .flex()
            .h_full()
            .w(px(252.0))
            .flex_col()
            .border_r_1()
            .border_color(theme.title_bar_border)
            .bg(theme.title_bar)
            .when(rounded_window, |this| {
                this.rounded_bl(theme.radius_lg).overflow_hidden()
            })
            .p_3()
            .child(self.render_title_row(cx, on_back))
            .child(div().h(px(12.0)).flex_none())
            .gap_1()
            .child(sidebar_nav_item(
                "home-section",
                "icons/home.svg",
                HomeSection::Home.title(),
                self.active_section == HomeSection::Home,
                cx,
                on_home,
            ))
            .child(sidebar_nav_item(
                "favorites-section",
                "icons/heart.svg",
                HomeSection::Favorites.title(),
                self.active_section == HomeSection::Favorites,
                cx,
                on_favorites,
            ))
            .child(sidebar_nav_item(
                "search-section",
                "icons/search.svg",
                HomeSection::Search.title(),
                self.active_section == HomeSection::Search,
                cx,
                on_search,
            ))
            .child(div().my_3().h(px(1.0)).bg(theme.title_bar_border))
            .child(div().flex().min_h_0().flex_1().flex_col().gap_1().children(
                self.servers.iter().map(|server| {
                    server_list_item(
                        server_title(server),
                        server.id == self.current_server.id,
                        cx,
                    )
                }),
            ))
            .child(user_row(username, cx))
    }

    fn render_title_row(
        &self,
        cx: &Context<HomePage>,
        on_back: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    ) -> impl IntoElement {
        let theme = theme::get(cx);

        div()
            .relative()
            .flex()
            .h(px(36.0))
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("home-back")
                    .absolute()
                    .left_0()
                    .flex()
                    .size(px(32.0))
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .hover(move |style| style.bg(theme.secondary_hover))
                    .child(
                        svg()
                            .path("icons/chevron-left.svg")
                            .size(px(18.0))
                            .text_color(theme.foreground),
                    )
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_click(on_back),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child("Tiny"),
            )
    }
}

impl Render for HomePage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.load_user_views_if_needed(cx);
        let theme = theme::get(cx);
        let on_back = cx.listener(Self::back_to_servers);
        let on_home = cx.listener(Self::select_home_section);
        let on_favorites = cx.listener(Self::select_favorites_section);
        let on_search = cx.listener(Self::select_search_section);

        div()
            .flex()
            .flex_1()
            .min_h_0()
            .size_full()
            .bg(theme.background)
            .child(self.render_sidebar(cx, false, on_back, on_home, on_favorites, on_search))
            .child(self.render_main_content(cx, false))
    }
}

fn user_view_card(
    name: String,
    image_path: Option<PathBuf>,
    cx: &Context<HomePage>,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let has_image = image_path.is_some();

    div()
        .flex()
        .w(px(180.0))
        .flex_col()
        .gap_2()
        .child(
            div()
                .h(px(104.0))
                .w_full()
                .overflow_hidden()
                .rounded_lg()
                .border_1()
                .border_color(theme.title_bar_border)
                .bg(theme.secondary_hover)
                .when_some(image_path, |this, path| {
                    this.child(img(path).size_full().object_fit(ObjectFit::Cover))
                })
                .when(!has_image, |this| {
                    this.flex()
                        .items_center()
                        .justify_center()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("暂无图片")
                }),
        )
        .child(
            div()
                .truncate()
                .text_sm()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.foreground)
                .child(name),
        )
}

fn sidebar_nav_item(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    active: bool,
    cx: &Context<HomePage>,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let color = if active {
        theme.foreground
    } else {
        theme.muted_foreground
    };

    div()
        .id(id)
        .flex()
        .h(px(34.0))
        .items_center()
        .gap_2()
        .rounded_md()
        .px_3()
        .text_sm()
        .text_color(color)
        .when(active, |this| this.bg(theme.secondary_hover))
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(svg().path(icon).size(px(16.0)).text_color(color))
        .child(label)
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .on_click(move |event, window, cx| {
            cx.stop_propagation();
            on_click(event, window, cx);
        })
}

fn server_list_item(title: String, active: bool, cx: &Context<HomePage>) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .flex()
        .h(px(30.0))
        .items_center()
        .rounded_md()
        .px_3()
        .text_sm()
        .text_color(if active {
            theme.foreground
        } else {
            theme.muted_foreground
        })
        .when(active, |this| this.bg(theme.secondary_hover))
        .child(title)
}

fn user_row(username: String, cx: &Context<HomePage>) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .mt_3()
        .flex()
        .h(px(38.0))
        .items_center()
        .justify_between()
        .gap_2()
        .rounded_md()
        .px_3()
        .text_sm()
        .text_color(theme.foreground)
        .child(
            div()
                .flex()
                .min_w_0()
                .items_center()
                .gap_2()
                .child(
                    svg()
                        .path("icons/user.svg")
                        .size(px(24.0))
                        .text_color(theme.foreground),
                )
                .child(div().truncate().child(username)),
        )
        .child(
            svg()
                .path("icons/setting.svg")
                .size(px(17.0))
                .text_color(theme.muted_foreground),
        )
}

fn server_title(server: &CachedServer) -> String {
    server
        .server_name
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or(&server.endpoint.address)
        .to_string()
}
