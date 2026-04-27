use std::{
    collections::{HashMap, HashSet},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, anyhow};
use gpui::{
    AppContext, ClickEvent, Context, CursorStyle, Entity, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Render, ResizeEdge, SharedString, Styled, Window, div,
    prelude::FluentBuilder, px, rgb, svg,
};
use uuid::Uuid;

use crate::{
    add_server_dialog::AddServerDialogState,
    emby::{EmbyClient, ItemCounts, PublicSystemInfo},
    home::{HomeEvent, HomePage},
    server::{AddServerSubmission, CachedItemCounts, CachedServer},
    storage::{self, ServerCache},
    theme,
    titlebar::app_titlebar,
};

pub struct TinyApp {
    add_server_dialog: Option<Entity<AddServerDialogState>>,
    open_server_menu: Option<String>,
    cache: ServerCache,
    servers: Vec<CachedServer>,
    cache_error: Option<SharedString>,
    item_counts: HashMap<String, ItemCounts>,
    item_counts_loading: HashSet<String>,
    item_counts_failed: HashSet<String>,
    item_counts_refreshed: HashSet<String>,
    window_bounds_observed: bool,
    window_persistence_enabled: bool,
    page: Page,
}

#[derive(Clone, Debug)]
enum Page {
    Servers,
    Home(Entity<HomePage>),
}

impl TinyApp {
    pub fn new(cache: ServerCache, cache_error: Option<SharedString>) -> Self {
        let servers = cache.servers.clone();
        let window_persistence_enabled = cache_error.is_none();
        let item_counts = cached_item_counts_by_server(&servers);

        Self {
            add_server_dialog: None,
            open_server_menu: None,
            cache,
            servers,
            cache_error,
            item_counts,
            item_counts_loading: HashSet::new(),
            item_counts_failed: HashSet::new(),
            item_counts_refreshed: HashSet::new(),
            window_bounds_observed: false,
            window_persistence_enabled,
            page: Page::Servers,
        }
    }

    fn observe_window_bounds_once(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.window_bounds_observed {
            return;
        }

        cx.observe_window_bounds(window, |app, window, cx| {
            app.save_window_size(window, cx);
        })
        .detach();
        self.window_bounds_observed = true;
    }

    fn title(&self) -> SharedString {
        match &self.page {
            Page::Servers => "Tiny".into(),
            Page::Home(_) => "首页".into(),
        }
    }

    fn save_window_size(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.window_persistence_enabled || window.is_maximized() || window.is_fullscreen() {
            return;
        }

        let size = window.window_bounds().get_bounds().size;
        let width = f32::from(size.width).round();
        let height = f32::from(size.height).round();
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return;
        }

        let width = width as u32;
        let height = height as u32;
        if !self.cache.set_window_size(width, height) {
            return;
        }

        if let Err(error) = storage::save(&self.cache) {
            self.cache_error = Some(format!("保存窗口大小失败：{error}").into());
            cx.notify();
        }
    }

    fn open_add_server_dialog(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.open_server_menu = None;
        if self.add_server_dialog.is_none() {
            self.add_server_dialog = Some(cx.new(AddServerDialogState::new));
        }
        cx.notify();
    }

    fn close_add_server_dialog(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.add_server_dialog = None;
        cx.notify();
    }

    fn close_server_menu(
        &mut self,
        _: &gpui::MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.open_server_menu.take().is_some() {
            cx.notify();
        }
    }

    fn show_servers_page_from_home(&mut self, cx: &mut Context<Self>) {
        self.open_server_menu = None;
        self.page = Page::Servers;
        cx.notify();
    }

    fn submit_add_server_dialog(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(dialog) = self.add_server_dialog.clone() else {
            return;
        };

        let Some(submission) = dialog.update(cx, |dialog, cx| dialog.submit(cx)) else {
            return;
        };

        dialog.update(cx, |dialog, cx| {
            dialog.clear_form_error(cx);
            dialog.set_submitting(true, cx);
        });

        let device_id = self.cache.device_id.clone();
        let cache = self.cache.clone();
        if let Some(server_id) = dialog.read(cx).edit_server_id() {
            let task = cx.background_spawn(async move {
                fetch_public_info_and_update_cache(device_id, cache, server_id, submission)
            });

            cx.spawn(async move |app, cx| {
                let result = task.await;
                app.update(cx, |app, cx| app.finish_edit_server(dialog, result, cx))
                    .ok();
            })
            .detach();
        } else {
            let task = cx.background_spawn(async move {
                fetch_public_info_and_cache(device_id, cache, submission)
            });

            cx.spawn(async move |app, cx| {
                let result = task.await;
                app.update(cx, |app, cx| app.finish_add_server(dialog, result, cx))
                    .ok();
            })
            .detach();
        }
    }

    fn select_server(&mut self, server: &CachedServer, _: &mut Window, cx: &mut Context<Self>) {
        self.open_server_menu = None;
        let device_id = self.cache.device_id.clone();
        let server_id = server.id.clone();
        let submission = AddServerSubmission {
            endpoint: server.endpoint.clone(),
            username: server.username.clone(),
            password: server.password.clone(),
        };
        let task = cx.background_spawn(async move {
            let client = EmbyClient::new(device_id)?;
            client.authenticate_by_name(&submission)
        });

        cx.spawn(async move |app, cx| {
            let result = task.await;
            app.update(cx, |app, cx| {
                app.finish_select_server(server_id, result, cx)
            })
            .ok();
        })
        .detach();
    }

    fn toggle_server_menu(
        &mut self,
        server: &CachedServer,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_server_menu = if self.open_server_menu.as_deref() == Some(server.id.as_str()) {
            None
        } else {
            Some(server.id.clone())
        };
        cx.notify();
    }

    fn open_edit_server_dialog(
        &mut self,
        server: &CachedServer,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_server_menu = None;
        if let Some(server) = self.servers.iter().find(|cached| cached.id == server.id) {
            let server = server.clone();
            self.add_server_dialog = Some(cx.new(|cx| AddServerDialogState::new_edit(&server, cx)));
        }
        cx.notify();
    }

    fn delete_server(&mut self, server: &CachedServer, _: &mut Window, cx: &mut Context<Self>) {
        self.open_server_menu = None;
        let mut cache = self.cache.clone();
        if !storage::delete_server_by_id(&mut cache, &server.id) {
            cx.notify();
            return;
        }

        match storage::save(&cache) {
            Ok(()) => {
                self.servers = cache.servers.clone();
                self.cache = cache;
                self.retain_item_count_state();
                self.cache_error = None;
            }
            Err(error) => {
                self.cache_error = Some(format!("删除服务器失败：{error}").into());
            }
        }
        cx.notify();
    }

    fn finish_select_server(
        &mut self,
        server_id: String,
        result: Result<crate::emby::AuthSession>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(session) => {
                self.finish_authenticated_server(server_id.clone(), session, cx);
            }
            Err(error) => {
                self.cache_error = Some(format!("登录服务器失败：{error}").into());
                self.page = Page::Servers;
            }
        }

        cx.notify();
    }

    fn finish_authenticated_server(
        &mut self,
        server_id: String,
        session: crate::emby::AuthSession,
        cx: &mut Context<Self>,
    ) {
        let user_id = session.user_id();
        let access_token = session.access_token;
        let mut updated_cache = false;

        if let Some(server) = self
            .servers
            .iter_mut()
            .find(|server| server.id == server_id)
        {
            server.user_id = user_id.clone();
            server.access_token = Some(access_token.clone());
        }

        if let Some(server) = self.servers.iter().find(|server| server.id == server_id) {
            let home_page = HomePage::new(
                server.clone(),
                self.servers.clone(),
                self.cache.device_id.clone(),
            );
            let home_page = cx.new(|_| home_page);
            cx.subscribe(&home_page, |app: &mut TinyApp, _, event, cx| match event {
                HomeEvent::BackToServers => app.show_servers_page_from_home(cx),
            })
            .detach();
            self.page = Page::Home(home_page);
        }

        if let Some(server) = self
            .cache
            .servers
            .iter_mut()
            .find(|server| server.id == server_id)
        {
            server.user_id = user_id;
            server.access_token = Some(access_token);
            updated_cache = true;
        }

        if !updated_cache {
            self.cache_error = Some("保存登录信息失败：服务器不存在".into());
            return;
        }

        self.item_counts_loading.remove(&server_id);
        self.item_counts_failed.remove(&server_id);
        self.item_counts_refreshed.remove(&server_id);

        match storage::save(&self.cache) {
            Ok(()) => {
                self.cache_error = None;
                self.item_counts_failed.remove(&server_id);
            }
            Err(error) => {
                self.cache_error = Some(format!("保存登录信息失败：{error}").into());
            }
        }
    }

    fn load_item_counts_for_authenticated_servers(&mut self, cx: &mut Context<Self>) {
        let device_id = self.cache.device_id.clone();
        let servers = self.servers.clone();

        for server in servers {
            if !has_cached_auth(&server)
                || self.item_counts_loading.contains(&server.id)
                || self.item_counts_failed.contains(&server.id)
                || self.item_counts_refreshed.contains(&server.id)
            {
                continue;
            }

            let server_id = server.id.clone();
            self.item_counts_loading.insert(server_id.clone());

            let device_id = device_id.clone();
            let task = cx.background_spawn(async move {
                let client = EmbyClient::new(device_id)?;
                client.item_counts(&server)
            });

            cx.spawn(async move |app, cx| {
                let result = task.await;
                app.update(cx, |app, cx| {
                    app.finish_item_counts(server_id, result, cx);
                })
                .ok();
            })
            .detach();
        }
    }

    fn finish_item_counts(
        &mut self,
        server_id: String,
        result: Result<ItemCounts>,
        cx: &mut Context<Self>,
    ) {
        self.item_counts_loading.remove(&server_id);

        match result {
            Ok(counts) => {
                self.item_counts_failed.remove(&server_id);
                self.item_counts_refreshed.insert(server_id.clone());
                self.update_item_counts_cache(server_id, counts);
            }
            Err(_) => {
                self.item_counts_failed.insert(server_id);
            }
        }

        cx.notify();
    }

    fn update_item_counts_cache(&mut self, server_id: String, counts: ItemCounts) {
        let cached_counts = CachedItemCounts {
            movie_count: counts.movie_count,
            series_count: counts.series_count,
        };

        if let Some(server) = self
            .servers
            .iter_mut()
            .find(|server| server.id == server_id)
        {
            server.item_counts = Some(cached_counts.clone());
        }

        if let Some(server) = self
            .cache
            .servers
            .iter_mut()
            .find(|server| server.id == server_id)
        {
            server.item_counts = Some(cached_counts);
            if let Err(error) = storage::save(&self.cache) {
                self.cache_error = Some(format!("保存媒体数量缓存失败：{error}").into());
            }
        }

        self.item_counts.insert(server_id, counts);
    }

    fn finish_add_server(
        &mut self,
        dialog: Entity<AddServerDialogState>,
        result: Result<(ServerCache, CachedServer, AddServerSubmission)>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok((cache, _, _)) => {
                self.servers = cache.servers.clone();
                self.cache = cache;
                self.retain_item_count_state();
                self.cache_error = None;
                self.add_server_dialog = None;
                self.page = Page::Servers;
            }
            Err(error) => {
                dialog.update(cx, |dialog, cx| {
                    dialog.set_submitting(false, cx);
                    dialog.set_form_error(format!("添加服务器失败：{error}"), cx);
                });
            }
        }

        cx.notify();
    }

    fn finish_edit_server(
        &mut self,
        dialog: Entity<AddServerDialogState>,
        result: Result<(ServerCache, CachedServer)>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok((cache, _)) => {
                self.servers = cache.servers.clone();
                self.cache = cache;
                self.retain_item_count_state();
                self.cache_error = None;
                self.add_server_dialog = None;
            }
            Err(error) => {
                dialog.update(cx, |dialog, cx| {
                    dialog.set_submitting(false, cx);
                    dialog.set_form_error(format!("保存服务器失败：{error}"), cx);
                });
            }
        }

        cx.notify();
    }

    fn retain_item_count_state(&mut self) {
        let authenticated_ids = self
            .servers
            .iter()
            .filter(|server| has_cached_auth(server))
            .map(|server| server.id.clone())
            .collect::<HashSet<_>>();

        self.item_counts
            .retain(|server_id, _| authenticated_ids.contains(server_id));
        self.item_counts_loading
            .retain(|server_id| authenticated_ids.contains(server_id));
        self.item_counts_failed
            .retain(|server_id| authenticated_ids.contains(server_id));
        self.item_counts_refreshed
            .retain(|server_id| authenticated_ids.contains(server_id));
    }

    fn render_content(
        &mut self,
        _rounded_window: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let error_color = theme::get(cx).error;
        let close_menu = cx.listener(Self::close_server_menu);
        let home_page = match &self.page {
            Page::Home(page) => Some(page.clone()),
            Page::Servers => None,
        };

        div()
            .relative()
            .flex_1()
            .min_h_0()
            .on_mouse_down(MouseButton::Left, close_menu)
            .when(matches!(self.page, Page::Servers), |this| {
                this.child(self.render_servers_page(cx))
            })
            .when_some(home_page, |this, page| this.child(page))
            .when_some(self.cache_error.clone(), |this, error| {
                this.child(
                    div()
                        .absolute()
                        .top(px(52.0))
                        .left(px(16.0))
                        .text_sm()
                        .text_color(error_color)
                        .child(error),
                )
            })
    }

    fn render_servers_page(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        self.load_item_counts_for_authenticated_servers(cx);

        let theme = theme::get(cx);

        div()
            .relative()
            .flex_1()
            .min_h_0()
            .size_full()
            .p_4()
            .when(self.servers.is_empty(), |this| {
                this.child(
                    div()
                        .absolute()
                        .top_0()
                        .right_0()
                        .bottom_0()
                        .left_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_base()
                        .text_color(theme.muted_foreground)
                        .child("点击右上角 + 添加 Emby 服务器"),
                )
            })
            .when(!self.servers.is_empty(), |this| {
                this.child(div().flex().flex_wrap().gap_3().children(
                    self.servers.iter().cloned().map(|server| {
                        let menu_open =
                            self.open_server_menu.as_deref() == Some(server.id.as_str());
                        let counts = self.item_counts.get(&server.id).cloned();
                        let select_server = cx.listener(Self::select_server);
                        let toggle_menu = cx.listener(Self::toggle_server_menu);
                        let edit_server = cx.listener(Self::open_edit_server_dialog);
                        let delete_server = cx.listener(Self::delete_server);
                        server_card(
                            server,
                            counts,
                            menu_open,
                            cx,
                            ServerCardActions {
                                on_select: select_server,
                                on_menu_toggle: toggle_menu,
                                on_edit: edit_server,
                                on_delete: delete_server,
                            },
                        )
                    }),
                ))
            })
    }
}

struct ServerCardActions<Select, Toggle, Edit, Delete> {
    on_select: Select,
    on_menu_toggle: Toggle,
    on_edit: Edit,
    on_delete: Delete,
}

fn server_card<Select, Toggle, Edit, Delete>(
    server: CachedServer,
    counts: Option<ItemCounts>,
    menu_open: bool,
    cx: &Context<TinyApp>,
    actions: ServerCardActions<Select, Toggle, Edit, Delete>,
) -> impl IntoElement
where
    Select: Fn(&CachedServer, &mut Window, &mut gpui::App) + 'static,
    Toggle: Fn(&CachedServer, &mut Window, &mut gpui::App) + 'static,
    Edit: Fn(&CachedServer, &mut Window, &mut gpui::App) + 'static,
    Delete: Fn(&CachedServer, &mut Window, &mut gpui::App) + 'static,
{
    let theme = theme::get(cx);
    let ServerCardActions {
        on_select,
        on_menu_toggle,
        on_edit,
        on_delete,
    } = actions;
    let title = server
        .server_name
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or(&server.endpoint.address)
        .to_string();
    let selected_server = server.clone();
    let menu_server = server.clone();

    div()
        .relative()
        .w(px(260.0))
        .child(
            div()
                .relative()
                .flex()
                .flex_col()
                .w(px(230.0))
                .h(px(100.0))
                .gap_2()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.input_border)
                .bg(theme.dialog_background)
                .p_4()
                .hover(move |style| style.bg(theme.secondary_hover))
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    cx.stop_propagation();
                    on_select(&selected_server, window, cx);
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_lg()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child(
                            svg()
                                .path("icons/emby.svg")
                                .size(px(32.0))
                                .text_color(gpui::Hsla::from(rgb(0x53b34c))),
                        )
                        .child(title),
                )
                .child(
                    div()
                        .flex()
                        .h(px(26.0))
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(div().flex().min_w_0().when_some(counts, |this, counts| {
                            this.child(server_counts_row(counts, cx))
                        }))
                        .child(server_menu_button(menu_server.clone(), cx, on_menu_toggle)),
                ),
        )
        .when(menu_open, |this| {
            this.child(server_card_menu(menu_server, cx, on_edit, on_delete))
        })
}

fn server_counts_row(counts: ItemCounts, cx: &Context<TinyApp>) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .flex()
        .h(px(26.0))
        .items_center()
        .gap_3()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(server_count_item(
            "icons/film.svg",
            counts.movie_count,
            theme.muted_foreground,
        ))
        .child(server_count_item(
            "icons/tv.svg",
            counts.series_count,
            theme.muted_foreground,
        ))
}

fn server_count_item(icon: &'static str, value: u32, color: gpui::Hsla) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(svg().path(icon).size(px(13.0)).text_color(color))
        .child(format!("{value}"))
}

fn has_cached_auth(server: &CachedServer) -> bool {
    server
        .user_id
        .as_deref()
        .is_some_and(|user_id| !user_id.is_empty())
        && server
            .access_token
            .as_deref()
            .is_some_and(|access_token| !access_token.is_empty())
}

fn cached_item_counts_by_server(servers: &[CachedServer]) -> HashMap<String, ItemCounts> {
    servers
        .iter()
        .filter_map(|server| {
            server
                .item_counts
                .as_ref()
                .map(|counts| (server.id.clone(), ItemCounts::from(counts)))
        })
        .collect()
}

fn server_menu_button(
    server: CachedServer,
    cx: &Context<TinyApp>,
    on_menu_toggle: impl Fn(&CachedServer, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .flex()
        .size(px(26.0))
        .flex_none()
        .items_center()
        .justify_center()
        .rounded_md()
        .text_color(theme.foreground)
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(
            svg()
                .path("icons/ellipsis.svg")
                .size(px(16.0))
                .text_color(theme.foreground),
        )
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            on_menu_toggle(&server, window, cx);
        })
}

fn server_card_menu(
    server: CachedServer,
    cx: &Context<TinyApp>,
    on_edit: impl Fn(&CachedServer, &mut Window, &mut gpui::App) + 'static,
    on_delete: impl Fn(&CachedServer, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let edit_server = server.clone();
    let delete_server = server;

    div()
        .absolute()
        .right(px(10.0))
        .bottom(px(-58.0))
        .flex()
        .flex_col()
        .w(px(112.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.input_border_focused)
        .bg(theme.dialog_background)
        .shadow_lg()
        .p(px(4.0))
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(menu_item(
            "编辑",
            false,
            move |window, cx| {
                on_edit(&edit_server, window, cx);
            },
            cx,
        ))
        .child(menu_item(
            "删除",
            true,
            move |window, cx| {
                on_delete(&delete_server, window, cx);
            },
            cx,
        ))
}

fn menu_item(
    label: &'static str,
    destructive: bool,
    action: impl Fn(&mut Window, &mut gpui::App) + 'static,
    cx: &Context<TinyApp>,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .flex()
        .h(px(30.0))
        .items_center()
        .rounded(px(6.0))
        .px_2()
        .text_sm()
        .text_color(if destructive {
            theme.error
        } else {
            theme.foreground
        })
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(label)
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            action(window, cx);
        })
}

fn fetch_public_info_and_cache(
    device_id: String,
    mut cache: ServerCache,
    submission: AddServerSubmission,
) -> Result<(ServerCache, CachedServer, AddServerSubmission)> {
    let client = EmbyClient::new(device_id)?;
    let info = client.public_system_info(&submission)?;
    let server = public_info_to_cached_server(
        Uuid::new_v4().to_string(),
        &submission,
        info,
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
    );
    storage::upsert_server(&mut cache, server.clone());
    storage::save(&cache)?;

    Ok((cache, server, submission))
}

fn fetch_public_info_and_update_cache(
    device_id: String,
    mut cache: ServerCache,
    server_id: String,
    submission: AddServerSubmission,
) -> Result<(ServerCache, CachedServer)> {
    let existing = cache
        .servers
        .iter()
        .find(|server| server.id == server_id)
        .cloned()
        .ok_or_else(|| anyhow!("服务器不存在"))?;
    let client = EmbyClient::new(device_id)?;
    let info = client.public_system_info(&submission)?;
    let server =
        public_info_to_cached_server(existing.id, &submission, info, existing.added_at_unix);
    if !storage::update_server_by_id(&mut cache, server.clone()) {
        return Err(anyhow!("服务器不存在"));
    }
    storage::save(&cache)?;

    Ok((cache, server))
}

fn public_info_to_cached_server(
    id: String,
    submission: &AddServerSubmission,
    info: PublicSystemInfo,
    added_at_unix: u64,
) -> CachedServer {
    CachedServer {
        id,
        endpoint: submission.endpoint.clone(),
        username: submission.username.clone(),
        password: submission.password.clone(),
        user_id: None,
        server_id: info.id,
        server_name: info.server_name,
        access_token: None,
        item_counts: None,
        added_at_unix,
    }
}

impl Render for TinyApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.observe_window_bounds_once(window, cx);

        let theme = theme::get(cx);
        let title = self.title();
        let add_server = cx.listener(Self::open_add_server_dialog);
        let close_dialog = cx.listener(Self::close_add_server_dialog);
        let close_menu = cx.listener(Self::close_server_menu);
        let submit_dialog = cx.listener(Self::submit_add_server_dialog);
        let dialog = self.add_server_dialog.clone();
        let modal_open = dialog.is_some();

        div()
            .relative()
            .size_full()
            .when(!window.is_maximized(), |this| {
                this.rounded(theme.radius_lg).overflow_hidden()
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .bg(theme.background)
                    .when(!window.is_maximized(), |this| {
                        this.rounded(theme.radius_lg).overflow_hidden()
                    })
                    .child(
                        div()
                            .on_mouse_down(MouseButton::Left, close_menu)
                            .child(app_titlebar(window, cx, title, add_server)),
                    )
                    .child(self.render_content(!window.is_maximized(), cx)),
            )
            .when_some(dialog, |this, dialog| {
                this.child(dialog.read(cx).render_layer(
                    dialog.clone(),
                    !window.is_maximized(),
                    close_dialog,
                    submit_dialog,
                    cx,
                ))
            })
            .when(!window.is_maximized() && !modal_open, |this| {
                this.children(resize_handles())
            })
    }
}

fn resize_handles() -> Vec<gpui::Div> {
    let edge_size = px(6.0);
    let corner_size = px(12.0);

    vec![
        resize_handle(ResizeEdge::Top, CursorStyle::ResizeUpDown)
            .top_0()
            .left(corner_size)
            .right(corner_size)
            .h(edge_size),
        resize_handle(ResizeEdge::Right, CursorStyle::ResizeLeftRight)
            .top(corner_size)
            .right_0()
            .bottom(corner_size)
            .w(edge_size),
        resize_handle(ResizeEdge::Bottom, CursorStyle::ResizeUpDown)
            .left(corner_size)
            .right(corner_size)
            .bottom_0()
            .h(edge_size),
        resize_handle(ResizeEdge::Left, CursorStyle::ResizeLeftRight)
            .top(corner_size)
            .left_0()
            .bottom(corner_size)
            .w(edge_size),
        resize_handle(ResizeEdge::TopLeft, CursorStyle::ResizeUpLeftDownRight)
            .top_0()
            .left_0()
            .size(corner_size),
        resize_handle(ResizeEdge::TopRight, CursorStyle::ResizeUpRightDownLeft)
            .top_0()
            .right_0()
            .size(corner_size),
        resize_handle(ResizeEdge::BottomRight, CursorStyle::ResizeUpLeftDownRight)
            .right_0()
            .bottom_0()
            .size(corner_size),
        resize_handle(ResizeEdge::BottomLeft, CursorStyle::ResizeUpRightDownLeft)
            .left_0()
            .bottom_0()
            .size(corner_size),
    ]
}

fn resize_handle(edge: ResizeEdge, cursor: CursorStyle) -> gpui::Div {
    div().absolute().flex_none().cursor(cursor).on_mouse_down(
        MouseButton::Left,
        move |_, window, cx| {
            cx.stop_propagation();
            window.start_window_resize(edge);
        },
    )
}
