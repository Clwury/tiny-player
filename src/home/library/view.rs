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
    favorites::controller::FavoritesIntent,
    library::{
        ItemsSortTarget, ItemsSortView, LibraryView, available_library_sorts,
        controller::{LibraryIntent, LibrarySource},
    },
    navigation::HomeRoute,
};

use crate::home::grid::*;

const LIBRARY_FIXED_HEADER_HEIGHT_PX: f32 = 64.0;
const LIBRARY_SORT_SELECT_WIDTH_PX: f32 = 232.0;
const LIBRARY_SORT_OPTION_HEIGHT_PX: f32 = 32.0;
const LIBRARY_SORT_TEXT_SIZE_PX: f32 = 14.0;
const LIBRARY_SORT_TEXT_LINE_HEIGHT_PX: f32 = 20.0;
const LIBRARY_SORT_ORDERS: [SortOrder; 2] = [SortOrder::Ascending, SortOrder::Descending];
// Movie/series cards are 302px tall at the default rem size (240px artwork,
// 8px total padding, an 8px card gap, and two metadata lines). Keeping the
// measured height here preserves the original flex-grid row spacing exactly.
impl HomeContent {
    pub(in crate::home) fn render_library_scrollable_content(
        &self,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
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
        self.render_items_feed_content(
            &LibrarySource::View(view_id.clone()),
            state,
            None,
            "该媒体库暂无内容",
            cx,
        )
    }

    pub(in crate::home) fn render_items_feed_content(
        &self,
        source: &LibrarySource,
        state: LibraryView<'_>,
        favorite_button: Option<gpui::AnyElement>,
        empty_message: &'static str,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = theme::get(cx);
        let (content_id, header_id, scroll_id, card_prefix) = match source {
            LibrarySource::View(_) => (
                "home-library-content",
                "home-library-fixed-header",
                "home-library-scroll-content",
                "library-grid-item",
            ),
            LibrarySource::Person(_) => (
                "home-person-content",
                "home-person-fixed-header",
                "home-person-scroll-content",
                "person-grid-item",
            ),
            LibrarySource::Genre(_) => (
                "home-genre-content",
                "home-genre-fixed-header",
                "home-genre-scroll-content",
                "genre-grid-item",
            ),
        };
        let total = state
            .model
            .paged
            .total_record_count
            .map(|total| format!("共 {total} 项"));
        let sort_select = self.render_library_sort_select(source, &state, cx);
        let back = cx.listener(Self::close_series_detail);

        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .id(content_id)
            .flex()
            .flex_col()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|page, _, _, cx| page.close_current_items_sort_menu(cx)),
            )
            .child(
                div()
                    .id(header_id)
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
                            .child(div().min_w_0().flex().items_center().gap_3().when_some(
                                total,
                                |this, total| {
                                    this.child(
                                        div()
                                            .flex_none()
                                            .text_sm()
                                            .text_color(theme.muted_foreground)
                                            .child(total),
                                    )
                                },
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_none()
                                    .items_center()
                                    .gap_3()
                                    .when_some(favorite_button, |this, button| this.child(button))
                                    .child(sort_select),
                            ),
                    ),
            )
            .child(
                div()
                    .id(scroll_id)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .scrollbar_width(px(HOME_MAIN_SCROLLBAR_GUTTER_PX))
                    .track_scroll(&state.presentation.grid.scroll_handle)
                    .px(px(HOME_MAIN_CONTENT_HORIZONTAL_PADDING_PX))
                    .pr(px(HOME_MAIN_SCROLL_CONTENT_RIGHT_PADDING_PX))
                    .pb_6()
                    .when(state.model.empty, |this| {
                        this.child(self.render_center_message(empty_message, false, cx))
                    })
                    .when(!state.model.paged.items.is_empty(), |this| {
                        this.child(self.render_items_grid(
                            &state.model.paged.items,
                            card_prefix,
                            cx,
                        ))
                        .child(self.render_library_paged_footer(&state, source, cx))
                    }),
            )
    }
    pub(in crate::home) fn render_library_sort_select(
        &self,
        source: &LibrarySource,
        state: &LibraryView<'_>,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        self.render_items_sort_select(
            ItemsSortView {
                options: state.model.sort_options,
                sort_by: state.model.sort_by,
                sort_order: state.model.sort_order,
                menu_open: state.presentation.sort_menu_open,
            },
            ItemsSortTarget::Library(source.clone()),
            cx,
        )
    }

    pub(in crate::home) fn render_items_sort_select(
        &self,
        state: ItemsSortView,
        target: ItemsSortTarget,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = theme::get(cx);
        let sort_by = state.sort_by;
        let sort_order = state.sort_order;
        let menu_open = state.menu_open;
        let trigger_label = format!(
            "{} · {}",
            library_sort_label(sort_by),
            library_sort_order_label(sort_order)
        );
        let toggle = cx.listener(|page, _, _, cx| page.toggle_current_items_sort_menu(cx));
        let sort_options = available_library_sorts(state.options)
            .enumerate()
            .map(|(index, candidate)| {
                let option_target = target.clone();
                let select = cx.listener(move |page, _, _, cx| match &option_target {
                    ItemsSortTarget::Library(source) => {
                        page.dispatch_items_source(source, LibraryIntent::SortBy(candidate), cx)
                    }
                    ItemsSortTarget::Favorites(item_type) => {
                        page.dispatch_favorites(*item_type, FavoritesIntent::SortBy(candidate), cx)
                    }
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
                .debug_selector(move || format!("library-sort-option-{index}"))
            })
            .collect::<Vec<_>>();
        let order_options = LIBRARY_SORT_ORDERS
            .into_iter()
            .enumerate()
            .map(|(index, candidate)| {
                let option_target = target.clone();
                let select = cx.listener(move |page, _, _, cx| match &option_target {
                    ItemsSortTarget::Library(source) => {
                        page.dispatch_items_source(source, LibraryIntent::SortOrder(candidate), cx)
                    }
                    ItemsSortTarget::Favorites(item_type) => page.dispatch_favorites(
                        *item_type,
                        FavoritesIntent::SortOrder(candidate),
                        cx,
                    ),
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
                .debug_selector(move || format!("library-sort-order-option-{index}"))
            })
            .collect::<Vec<_>>();
        div()
            .relative()
            .flex_none()
            .child(
                library_sort_trigger(trigger_label, menu_open, cx)
                    .id("library-sort-select")
                    .debug_selector(|| "library-sort-select".into())
                    .on_click(toggle),
            )
            .when(menu_open, |this| {
                this.child(
                    deferred(
                        div()
                            .id("library-sort-menu")
                            .debug_selector(|| "library-sort-menu".into())
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
                                    .gap(px(2.0))
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
                                    .gap(px(2.0))
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
        source: &LibrarySource,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        let page = cx.entity().downgrade();
        let auto_load_source = source.clone();
        let scroll_handle = state.presentation.grid.scroll_handle.clone();
        let auto_load_observer = canvas(
            |bounds, _, _| bounds,
            move |_, _, window, _| {
                if !workspace_scroll_is_near_end(&scroll_handle) {
                    return;
                }
                let page = page.clone();
                let source = auto_load_source.clone();
                window.on_next_frame(move |_, cx| {
                    page.update(cx, |page, cx| {
                        page.auto_load_more_items_source(&source, cx);
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
        .text_size(px(LIBRARY_SORT_TEXT_SIZE_PX))
        .line_height(px(LIBRARY_SORT_TEXT_LINE_HEIGHT_PX))
        .text_color(theme.foreground)
        .cursor_pointer()
        .hover(move |style| style.bg(theme.secondary_hover))
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(div().flex_1().min_w_0().truncate().child(label))
        .child(
            svg()
                .path("icons/chevron-up-down.svg")
                .size(px(14.0))
                .flex_none()
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
        .h(px(30.0))
        .flex_none()
        .items_center()
        .px_3()
        .text_size(px(12.0))
        .line_height(px(16.0))
        .font_weight(gpui::FontWeight::MEDIUM)
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
        .text_size(px(LIBRARY_SORT_TEXT_SIZE_PX))
        .line_height(px(LIBRARY_SORT_TEXT_LINE_HEIGHT_PX))
        .font_weight(if selected {
            gpui::FontWeight::MEDIUM
        } else {
            gpui::FontWeight::NORMAL
        })
        .text_color(if selected {
            theme.accent_text
        } else {
            theme.foreground
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
        .child(div().flex_1().min_w_0().truncate().child(label))
        .child(
            div()
                .flex()
                .size(px(16.0))
                .flex_none()
                .items_center()
                .justify_center()
                .when(selected, |this| {
                    this.child(
                        svg()
                            .path("icons/check.svg")
                            .size(px(16.0))
                            .text_color(theme.accent_text),
                    )
                }),
        )
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
