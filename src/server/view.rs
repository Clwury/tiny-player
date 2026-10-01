pub(crate) mod form;
use std::{rc::Rc, time::Duration};
pub(crate) mod reorder;

use gpui::{
    Animation, AnimationExt as _, App, AppContext as _, Context, ElementId, EntityId, FocusHandle,
    Hsla, InteractiveElement, IntoElement, MouseButton, ParentElement, Pixels, Point, Render,
    SharedString, StatefulInteractiveElement, Styled, Transformation, Window, anchored, div,
    percentage, point, prelude::FluentBuilder, px, svg,
};

use super::feature::{ServerCardVm, ServerMenuVm};
use crate::ui::radius;
use crate::{emby::ItemCounts, theme, ui::server_icon::server_icon};

/// View actions carry only IDs and presentation coordinates. The shell maps
/// these to domain intents and overlay lifecycle without giving UI credentials.
pub(crate) enum ServerViewIntent {
    Select(String),
    Add,
    OpenMenu {
        server_id: String,
        position: Point<Pixels>,
    },
    CloseMenu,
    Edit(String),
    ChooseIcon(String),
    Delete(String),
    ToggleAutoStart(String),
    BeginReorder(String),
    PreviewReorder(usize),
    CommitReorder(bool),
}

pub(crate) type ServerViewListener = Rc<dyn Fn(ServerViewIntent, &mut Window, &mut App)>;

pub(crate) struct ServerGridProps {
    pub(crate) cards: Vec<(ServerCardVm, reorder::CardPosition)>,
    pub(crate) owner: EntityId,
    pub(crate) can_reorder: bool,
    pub(crate) reorder_focus: Option<FocusHandle>,
}

pub(crate) fn server_page(
    props: ServerGridProps,
    on_intent: ServerViewListener,
    cx: &App,
) -> gpui::AnyElement {
    let on_key = on_intent.clone();
    let owner = props.owner;
    div()
        .relative()
        .flex_1()
        .min_h_0()
        .size_full()
        .p_4()
        .child(
            div()
                .id("server-grid")
                .flex()
                .flex_wrap()
                .gap_3()
                .when_some(props.reorder_focus, |this, focus| {
                    this.track_focus(&focus)
                        .on_key_down(move |event, window, cx| {
                            if event.keystroke.key == "escape" {
                                cx.stop_active_drag(window);
                                on_key(ServerViewIntent::CommitReorder(false), window, cx);
                                cx.stop_propagation();
                            }
                        })
                })
                .children(
                    props
                        .cards
                        .into_iter()
                        .enumerate()
                        .map(|(index, (vm, position))| {
                            let slot_id = (ElementId::from("server-slot"), vm.server_id.clone());
                            let slot_selector = format!("server-slot-{index}");
                            let placeholder = vm.placeholder;
                            let card =
                                server_card(vm, owner, props.can_reorder, on_intent.clone(), cx);
                            let on_move = on_intent.clone();
                            let on_drop = on_intent.clone();
                            div()
                                .id(slot_id)
                                .debug_selector(move || slot_selector.clone())
                                .flex_none()
                                .when(props.can_reorder, |this| {
                                    this.on_drag_move(
                                        move |event: &gpui::DragMoveEvent<DraggedServer>,
                                              window,
                                              cx| {
                                            if event.drag(cx).owner == owner
                                                && event.bounds.contains(&event.event.position)
                                            {
                                                on_move(
                                                    ServerViewIntent::PreviewReorder(index),
                                                    window,
                                                    cx,
                                                );
                                            }
                                        },
                                    )
                                    .on_drop(
                                        move |drag: &DraggedServer, window, cx| {
                                            if drag.owner == owner {
                                                on_drop(
                                                    ServerViewIntent::PreviewReorder(index),
                                                    window,
                                                    cx,
                                                );
                                                on_drop(
                                                    ServerViewIntent::CommitReorder(true),
                                                    window,
                                                    cx,
                                                );
                                            }
                                        },
                                    )
                                })
                                .child(reorder::animated_card(
                                    card.into_any_element(),
                                    position,
                                    index,
                                    placeholder,
                                ))
                        }),
                )
                .child(add_server_card(cx, on_intent.clone())),
        )
        .into_any_element()
}

pub(crate) const SERVER_CARD_WIDTH_PX: f32 = 190.0;
pub(crate) const SERVER_CARD_HEIGHT_PX: f32 = 96.0;
const SERVER_CARD_LOADER_ANIMATION_MS: u64 = 1800;

#[derive(Clone)]
pub(crate) struct ServerContextMenu {
    pub(crate) position: Point<Pixels>,
    pub(crate) focus: FocusHandle,
    pub(crate) previous_focus: Option<FocusHandle>,
}

#[derive(Clone)]
pub(crate) struct DraggedServer {
    pub(crate) owner: EntityId,
    server_id: String,
    title: String,
    icon_url: Option<String>,
    counts: Option<ItemCounts>,
    auto_start: bool,
}

impl Render for DraggedServer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);

        div()
            .w(px(SERVER_CARD_WIDTH_PX))
            .h(px(SERVER_CARD_HEIGHT_PX))
            .rounded(radius::CARD)
            .border_1()
            .border_color(theme.accent)
            .bg(theme.dialog_background)
            .shadow_lg()
            .opacity(0.9)
            .child(server_card_content(
                self.title.clone(),
                self.icon_url.as_deref(),
                self.counts.clone(),
                None,
                self.auto_start,
                cx,
            ))
    }
}

pub(crate) fn add_server_card(cx: &App, on_intent: ServerViewListener) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .id("add-server-card")
        .flex()
        .flex_none()
        .w(px(SERVER_CARD_WIDTH_PX))
        .h(px(SERVER_CARD_HEIGHT_PX))
        .items_center()
        .justify_center()
        .rounded(radius::CARD)
        .border_1()
        .border_color(theme.input_border)
        .bg(theme.dialog_background)
        .text_color(theme.muted_foreground)
        .cursor_pointer()
        .hover(move |style| {
            style
                .bg(theme.secondary_hover)
                .border_color(theme.input_border_focused)
                .text_color(theme.foreground)
        })
        .child(
            svg()
                .path("icons/plus.svg")
                .size(px(24.0))
                .text_color(theme.foreground),
        )
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            on_intent(ServerViewIntent::Add, window, cx);
        })
}

pub(crate) fn server_card(
    card: ServerCardVm,
    owner: EntityId,
    can_reorder: bool,
    on_intent: ServerViewListener,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let ServerCardVm {
        server_id,
        title,
        icon_url,
        counts,
        loading,
        placeholder,
        auto_start,
    } = card;
    let selected_id = server_id.clone();
    let menu_server_id = server_id.clone();
    let selector = format!("server-card-{server_id}");
    let loader_server_id = loading.then(|| server_id.clone());
    let card_id = (ElementId::from("server-card"), server_id.clone());
    let drag = DraggedServer {
        owner,
        server_id,
        title: title.clone(),
        icon_url: icon_url.clone(),
        counts: counts.clone(),
        auto_start,
    };
    let on_select = on_intent.clone();
    let on_menu = on_intent.clone();

    div()
        .id(card_id)
        .debug_selector(move || selector.clone())
        .relative()
        .flex_none()
        .w(px(SERVER_CARD_WIDTH_PX))
        .h(px(SERVER_CARD_HEIGHT_PX))
        .rounded(radius::CARD)
        .border_1()
        .border_color(theme.input_border)
        .bg(theme.dialog_background)
        .when(placeholder, |this| {
            this.bg(theme.element_selected)
                .border_color(theme.accent)
                .opacity(0.3)
        })
        .cursor_default()
        .when(!loading, |this| this.cursor_pointer())
        .hover(move |style| {
            style
                .bg(theme.secondary_hover)
                .border_color(theme.input_border_focused)
        })
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            on_select(ServerViewIntent::Select(selected_id.clone()), window, cx);
        })
        .on_mouse_down(MouseButton::Right, move |event, window, cx| {
            cx.stop_propagation();
            if !loading && !cx.has_active_drag() {
                on_menu(
                    ServerViewIntent::OpenMenu {
                        server_id: menu_server_id.clone(),
                        position: event.position,
                    },
                    window,
                    cx,
                );
            }
        })
        .when(can_reorder, |this| {
            this.on_drag(drag, move |drag, _, window, cx| {
                on_intent(
                    ServerViewIntent::BeginReorder(drag.server_id.clone()),
                    window,
                    cx,
                );
                // GPUI installs the active drag after this callback returns.
                window.defer(cx, |window, cx| {
                    cx.set_active_drag_cursor_style(gpui::CursorStyle::ClosedHand, window);
                });
                cx.new(|_| drag.clone())
            })
        })
        .child(server_card_content(
            title,
            icon_url.as_deref(),
            counts,
            loader_server_id,
            auto_start,
            cx,
        ))
}

fn server_card_content(
    title: String,
    icon_url: Option<&str>,
    counts: Option<ItemCounts>,
    loading_server_id: Option<String>,
    auto_start: bool,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .relative()
        .flex()
        .flex_col()
        .size_full()
        .justify_between()
        .p_3()
        .child(
            div()
                .flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap_2()
                .text_base()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .child(server_icon(icon_url, 28.0))
                .child(div().flex_1().min_w_0().text_ellipsis().child(title)),
        )
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .h(px(20.0))
                .gap_2()
                .child(div().flex().min_w_0().when_some(counts, |this, counts| {
                    this.child(server_counts_row(counts, cx))
                }))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap_2()
                        .when_some(loading_server_id, |this, server_id| {
                            this.child(loader_icon(server_id, cx))
                        })
                        .when(auto_start, |this| {
                            this.child(
                                div()
                                    .id("server-auto-start")
                                    .debug_selector(|| "server-auto-start".into())
                                    .aria_label("自动启动")
                                    .flex()
                                    .flex_none()
                                    .size(px(16.0))
                                    .child(
                                        svg()
                                            .path("icons/zap.svg")
                                            .size_full()
                                            .text_color(theme.accent),
                                    ),
                            )
                        }),
                ),
        )
}

fn server_counts_row(counts: ItemCounts, cx: &App) -> impl IntoElement {
    let theme = theme::get(cx);

    div()
        .flex()
        .items_center()
        .gap_2()
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

fn loader_icon(server_id: String, cx: &App) -> impl IntoElement {
    let theme = theme::get(cx);
    let animation_id = SharedString::from(format!("server-card-loader-{server_id}"));

    svg()
        .path("icons/loader.svg")
        .size(px(16.0))
        .overflow_hidden()
        .text_color(theme.muted_foreground)
        .with_animation(
            animation_id,
            Animation::new(Duration::from_millis(SERVER_CARD_LOADER_ANIMATION_MS)).repeat(),
            |svg, delta| svg.with_transformation(Transformation::rotate(percentage(delta))),
        )
}

pub(crate) fn server_card_menu(
    vm: ServerMenuVm,
    menu: ServerContextMenu,
    on_intent: ServerViewListener,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let menu_id = ElementId::from(format!("server-card-menu-{}", vm.server_id));
    let edit_id = vm.server_id.clone();
    let icon_id = vm.server_id.clone();
    let auto_start_id = vm.server_id.clone();
    let delete_id = vm.server_id;
    let on_edit = on_intent.clone();
    let on_icon = on_intent.clone();
    let on_auto_start = on_intent.clone();
    let on_delete = on_intent.clone();
    let on_close_out = on_intent.clone();

    anchored()
        .position(menu.position)
        .offset(point(px(4.0), px(4.0)))
        .snap_to_window_with_margin(px(8.0))
        .child(
            div()
                .id("server-context-menu")
                .debug_selector(|| "server-context-menu".into())
                .track_focus(&menu.focus)
                .occlude()
                .cursor_default()
                .flex()
                .flex_col()
                .w(px(128.0))
                .rounded(radius::SURFACE)
                .border_1()
                .border_color(theme.context_menu.border)
                .bg(theme.context_menu.background)
                .shadow_lg()
                .p(px(4.0))
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .on_mouse_down_out(move |_, window, cx| {
                    on_close_out(ServerViewIntent::CloseMenu, window, cx)
                })
                .on_key_down(move |event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        cx.stop_propagation();
                        on_intent(ServerViewIntent::CloseMenu, window, cx);
                    }
                })
                .child(menu_item(
                    (menu_id.clone(), "edit"),
                    "编辑",
                    false,
                    move |window, cx| {
                        on_edit(ServerViewIntent::Edit(edit_id.clone()), window, cx);
                    },
                    cx,
                ))
                .child(menu_item(
                    (menu_id.clone(), "choose-icon"),
                    "选择图标",
                    false,
                    move |window, cx| {
                        on_icon(ServerViewIntent::ChooseIcon(icon_id.clone()), window, cx);
                    },
                    cx,
                ))
                .child(menu_item(
                    (menu_id.clone(), "auto-start"),
                    if vm.auto_start {
                        "取消自动启动"
                    } else {
                        "自动启动"
                    },
                    false,
                    move |window, cx| {
                        on_auto_start(
                            ServerViewIntent::ToggleAutoStart(auto_start_id.clone()),
                            window,
                            cx,
                        );
                    },
                    cx,
                ))
                .child(menu_item(
                    (menu_id, "delete"),
                    "删除",
                    true,
                    move |window, cx| {
                        on_delete(ServerViewIntent::Delete(delete_id.clone()), window, cx);
                    },
                    cx,
                )),
        )
}

fn menu_item(
    id: impl Into<ElementId>,
    label: &'static str,
    destructive: bool,
    action: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let colors = &theme::get(cx).context_menu;
    let hover_background = if destructive {
        colors.destructive_hover_background
    } else {
        colors.hover_background
    };

    div()
        .id(id)
        .debug_selector(move || format!("server-context-menu-{label}"))
        .cursor_pointer()
        .flex()
        .h(px(30.0))
        .items_center()
        .rounded(radius::CONTROL)
        .px_2()
        .text_sm()
        .text_color(if destructive {
            colors.destructive_foreground
        } else {
            colors.foreground
        })
        .hover(move |style| style.bg(hover_background))
        .child(label)
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            action(window, cx);
        })
}
