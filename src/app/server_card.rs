use gpui::{
    App, Context, Hsla, InteractiveElement, IntoElement, MouseButton, ParentElement, Styled,
    Window, div, prelude::FluentBuilder, px, rgb, svg,
};

use crate::{emby::ItemCounts, server::CachedServer, theme};

use super::TinyApp;

pub(super) struct ServerCardActions<Select, Toggle, Edit, Delete> {
    pub(super) on_select: Select,
    pub(super) on_menu_toggle: Toggle,
    pub(super) on_edit: Edit,
    pub(super) on_delete: Delete,
}

pub(super) fn server_card<Select, Toggle, Edit, Delete>(
    server: CachedServer,
    counts: Option<ItemCounts>,
    menu_open: bool,
    cx: &Context<TinyApp>,
    actions: ServerCardActions<Select, Toggle, Edit, Delete>,
) -> impl IntoElement
where
    Select: Fn(&CachedServer, &mut Window, &mut App) + 'static,
    Toggle: Fn(&CachedServer, &mut Window, &mut App) + 'static,
    Edit: Fn(&CachedServer, &mut Window, &mut App) + 'static,
    Delete: Fn(&CachedServer, &mut Window, &mut App) + 'static,
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
                                .text_color(Hsla::from(rgb(0x53b34c))),
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

fn server_count_item(icon: &'static str, value: u32, color: Hsla) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(svg().path(icon).size(px(13.0)).text_color(color))
        .child(format!("{value}"))
}

fn server_menu_button(
    server: CachedServer,
    cx: &Context<TinyApp>,
    on_menu_toggle: impl Fn(&CachedServer, &mut Window, &mut App) + 'static,
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
    on_edit: impl Fn(&CachedServer, &mut Window, &mut App) + 'static,
    on_delete: impl Fn(&CachedServer, &mut Window, &mut App) + 'static,
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
    action: impl Fn(&mut Window, &mut App) + 'static,
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
