use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, canvas, deferred, div, prelude::FluentBuilder, px, svg,
};

use crate::home::components::workspace_back_button;
use crate::ui::radius;
use crate::{
    emby::{SortOrder, UserItemsSort},
    theme,
};

use crate::home::{
    HomeContent,
    carousel::{
        HOME_MAIN_CONTENT_HORIZONTAL_PADDING_PX, HOME_MAIN_SCROLL_CONTENT_RIGHT_PADDING_PX,
        HOME_MAIN_SCROLLBAR_GUTTER_PX,
    },
    library::{LibraryView, available_library_sorts},
    navigation::HomeRoute,
};

use crate::home::grid::*;

const LIBRARY_FIXED_HEADER_HEIGHT_PX: f32 = 64.0;
const LIBRARY_SORT_SELECT_WIDTH_PX: f32 = 232.0;
const LIBRARY_SORT_OPTION_HEIGHT_PX: f32 = 30.0;
const LIBRARY_SORT_ORDERS: [SortOrder; 2] = [SortOrder::Ascending, SortOrder::Descending];
// Movie/series cards are 302px tall at the default rem size (240px artwork,
// 8px total padding, an 8px card gap, and two metadata lines). Keeping the
// measured height here preserves the original flex-grid row spacing exactly.
impl HomeContent {
    pub(in crate::home) fn render_library_scrollable_content(
        &self,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let view_id = match self.controller.route() {
            HomeRoute::Library { view_id, .. } => view_id,
            _ => unreachable!("library renderer requires a Library route"),
        };
        let state = LibraryView {
            model: self
                .controller
                .library_view(view_id)
                .expect("Library route has a controller"),
            presentation: &self
                .library_resources
                .get(view_id)
                .expect("Library route has presentation resources")
                .presentation,
        };
        let total = state
            .model
            .paged
            .total_record_count
            .map(|total| format!("共 {total} 项"));
        let sort_select = self.render_library_sort_select(view_id, &state, cx);
        let back = cx.listener(Self::close_series_detail);

        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .id("home-library-content")
            .flex()
            .flex_col()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|page, _, _, cx| page.close_current_library_sort_menu(cx)),
            )
            .child(
                div()
                    .id("home-library-fixed-header")
                    .flex()
                    .h(px(LIBRARY_FIXED_HEADER_HEIGHT_PX))
                    .flex_none()
                    .items_start()
                    .gap_3()
                    .bg(theme.background)
                    .px(px(HOME_MAIN_CONTENT_HORIZONTAL_PADDING_PX))
                    .pt_3()
                    .child(workspace_back_button(back, cx))
                    .child(
                        div()
                            .flex()
                            .h(px(36.0))
                            .flex_1()
                            .min_w_0()
                            .items_center()
                            .justify_between()
                            .gap_4()
                            .child(div().min_w_0().when_some(total, |this, total| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .text_color(theme.muted_foreground)
                                        .child(total),
                                )
                            }))
                            .child(sort_select),
                    ),
            )
            .child(
                div()
                    .id("home-library-scroll-content")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .scrollbar_width(px(HOME_MAIN_SCROLLBAR_GUTTER_PX))
                    .track_scroll(&state.presentation.grid.scroll_handle)
                    .px(px(HOME_MAIN_CONTENT_HORIZONTAL_PADDING_PX))
                    .pr(px(HOME_MAIN_SCROLL_CONTENT_RIGHT_PADDING_PX))
                    .pb_6()
                    .when(state.model.empty, |this| {
                        this.child(self.render_center_message("该媒体库暂无内容", false, cx))
                    })
                    .when(!state.model.paged.items.is_empty(), |this| {
                        this.child(self.render_items_grid(
                            &state.model.paged.items,
                            "library-grid-item",
                            cx,
                        ))
                        .child(self.render_library_paged_footer(&state, view_id, cx))
                    }),
            )
    }
    pub(in crate::home) fn render_library_sort_select(
        &self,
        view_id: &str,
        state: &LibraryView<'_>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        let sort_by = state.model.sort_by;
        let sort_order = state.model.sort_order;
        let menu_open = state.presentation.sort_menu_open;
        let trigger_label = format!(
            "{} · {}",
            library_sort_label(sort_by),
            library_sort_order_label(sort_order)
        );
        let toggle = cx.listener(|page, _, _, cx| page.toggle_current_library_sort_menu(cx));
        let sort_options = available_library_sorts(state.model.item_types)
            .enumerate()
            .map(|(index, candidate)| {
                let option_view_id = view_id.to_string();
                let select = cx.listener(move |page, _, _, cx| {
                    page.select_library_sort_by(option_view_id.clone(), candidate, cx);
                });
                library_sort_option(
                    library_sort_label(candidate),
                    candidate == sort_by,
                    (
                        gpui::ElementId::from("library-sort-option"),
                        index.to_string(),
                    ),
                    cx,
                )
                .on_click(select)
            })
            .collect::<Vec<_>>();
        let order_options = LIBRARY_SORT_ORDERS
            .into_iter()
            .enumerate()
            .map(|(index, candidate)| {
                let option_view_id = view_id.to_string();
                let select = cx.listener(move |page, _, _, cx| {
                    page.select_library_sort_order(option_view_id.clone(), candidate, cx);
                });
                library_sort_option(
                    library_sort_order_label(candidate),
                    candidate == sort_order,
                    (
                        gpui::ElementId::from("library-sort-order-option"),
                        index.to_string(),
                    ),
                    cx,
                )
                .on_click(select)
            })
            .collect::<Vec<_>>();
        div()
            .relative()
            .flex_none()
            .child(
                library_sort_trigger(trigger_label, menu_open, cx)
                    .id("library-sort-select")
                    .on_click(toggle),
            )
            .when(menu_open, |this| {
                this.child(
                    deferred(
                        div()
                            .id("library-sort-menu")
                            .cursor_default()
                            .absolute()
                            .top(px(40.0))
                            .right_0()
                            .flex()
                            .w(px(LIBRARY_SORT_SELECT_WIDTH_PX))
                            .flex_col()
                            .overflow_hidden()
                            .rounded(radius::SURFACE)
                            .border_1()
                            .border_color(theme.input_border_focused)
                            .bg(theme.dialog_background)
                            .shadow_lg()
                            .occlude()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .child(library_sort_group_label("排序方式", cx))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .px_1()
                                    .pb_1()
                                    .children(sort_options),
                            )
                            .child(div().mx_2().h(px(1.0)).bg(theme.input_border))
                            .child(library_sort_group_label("排列顺序", cx))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .px_1()
                                    .pb_1()
                                    .children(order_options),
                            ),
                    )
                    .with_priority(2),
                )
            })
    }
    pub(in crate::home) fn render_library_paged_footer(
        &self,
        state: &LibraryView<'_>,
        view_id: &str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let page = cx.entity().downgrade();
        let auto_load_view_id = view_id.to_string();
        let scroll_handle = state.presentation.grid.scroll_handle.clone();
        let auto_load_observer = canvas(
            |bounds, _, _| bounds,
            move |_, _, window, _| {
                if !workspace_scroll_is_near_end(&scroll_handle) {
                    return;
                }
                let page = page.clone();
                let view_id = auto_load_view_id.clone();
                window.on_next_frame(move |_, cx| {
                    page.update(cx, |page, cx| {
                        page.auto_load_more_library(&view_id, cx);
                    })
                    .ok();
                });
            },
        )
        .w_full()
        .h(px(1.0));

        div()
            .id("library-auto-load-footer")
            .mt_5()
            .flex()
            .min_h(px(1.0))
            .flex_col()
            .items_center()
            .gap_2()
            .when(state.model.paged.can_auto_load_more(), |this| {
                this.child(auto_load_observer)
            })
    }
}
pub(in crate::home) fn library_sort_trigger<T>(
    label: String,
    menu_open: bool,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .h(px(32.0))
        .w(px(LIBRARY_SORT_SELECT_WIDTH_PX))
        .items_center()
        .justify_between()
        .gap_2()
        .rounded(radius::CONTROL)
        .border_1()
        .border_color(if menu_open {
            theme.input_border_focused
        } else {
            theme.input_border
        })
        .bg(theme.dialog_background.opacity(0.88))
        .px_3()
        .text_sm()
        .text_color(theme.foreground)
        .cursor_pointer()
        .hover(move |style| style.bg(theme.secondary_hover))
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(div().min_w_0().truncate().child(label))
        .child(
            svg()
                .path("icons/chevron-right.svg")
                .size(px(14.0))
                .text_color(theme.muted_foreground),
        )
}

pub(in crate::home) fn library_sort_group_label<T>(
    label: &'static str,
    cx: &Context<T>,
) -> gpui::Div {
    let theme = theme::get(cx);

    div()
        .flex()
        .h(px(28.0))
        .flex_none()
        .items_center()
        .px_3()
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.muted_foreground)
        .child(label)
}

pub(in crate::home) fn library_sort_option<T>(
    label: &'static str,
    selected: bool,
    id: impl Into<gpui::ElementId>,
    cx: &Context<T>,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::get(cx);

    div()
        .id(id)
        .flex()
        .h(px(LIBRARY_SORT_OPTION_HEIGHT_PX))
        .flex_none()
        .items_center()
        .justify_between()
        .gap_2()
        .rounded(radius::CONTROL)
        .px_2()
        .text_sm()
        .font_weight(if selected {
            gpui::FontWeight::SEMIBOLD
        } else {
            gpui::FontWeight::NORMAL
        })
        .text_color(if selected {
            theme.accent_text
        } else {
            theme.muted_foreground
        })
        .bg(if selected {
            theme.element_selected
        } else {
            theme.dialog_background
        })
        .cursor_pointer()
        .hover(move |style| {
            style.bg(if selected {
                theme.element_selected_hover
            } else {
                theme.secondary_hover
            })
        })
        .child(div().min_w_0().truncate().child(label))
        .when(selected, |this| {
            this.child(
                svg()
                    .path("icons/check.svg")
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.input_border_focused),
            )
        })
}

pub(in crate::home) fn library_sort_label(sort_by: UserItemsSort) -> &'static str {
    match sort_by {
        UserItemsSort::SortName => "名称",
        UserItemsSort::DateCreated => "添加日期",
        UserItemsSort::PremiereDate => "发行日期",
        UserItemsSort::ProductionYear => "年份",
        UserItemsSort::CommunityRating => "社区评分",
        UserItemsSort::CriticRating => "影评人评分",
        UserItemsSort::DatePlayed => "播放日期",
        UserItemsSort::DateLastContentAdded => "最后一集添加日期",
        UserItemsSort::PlayCount => "播放次数",
        UserItemsSort::Random => "随机",
        UserItemsSort::OfficialRating => "分级",
    }
}

pub(in crate::home) fn library_sort_order_label(sort_order: SortOrder) -> &'static str {
    match sort_order {
        SortOrder::Ascending => "升序",
        SortOrder::Descending => "降序",
    }
}
