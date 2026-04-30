use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, ParentElement, Render, Styled, Window,
    div, prelude::FluentBuilder, px,
};

use crate::{theme, titlebar::app_titlebar};

use super::{
    Page, TinyApp,
    resize::resize_handles,
    server_card::{ServerCardActions, server_card},
};

impl TinyApp {
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

impl Render for TinyApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.observe_window_bounds_once(window, cx);

        let theme = theme::get(cx);
        let title = self.title(cx);
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
