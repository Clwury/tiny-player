//! Server icon picker presentation. Shell callbacks own IO and overlay lifetime.
use crate::{
    server::{feature::IconPickerVm, icon::Icon},
    theme,
    ui::{
        editor::{Editor, Escape},
        radius,
        scrollbar::Scrollbar,
        server_icon::icon_preview,
    },
};
use gpui::{
    App, Context, Corners, Entity, FocusHandle, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Pixels, StatefulInteractiveElement, Styled, UniformListScrollHandle, Window,
    div, prelude::FluentBuilder, px, svg, uniform_list,
};
use std::rc::Rc;
use uuid::Uuid;

pub(crate) enum IconPickerIntent {
    Close,
    Select(usize),
}
pub(crate) type IconPickerListener = Rc<dyn Fn(IconPickerIntent, &mut Window, &mut App)>;

pub(crate) struct IconPickerProps<'a> {
    pub(crate) session: Uuid,
    pub(crate) focus: FocusHandle,
    pub(crate) scroll: UniformListScrollHandle,
    pub(crate) search: Entity<Editor>,
    pub(crate) corners: Corners<Pixels>,
    pub(crate) catalog: &'static [Icon],
    pub(crate) view: IconPickerVm<'a>,
}

/// Read at virtual-list delivery after the caller validates the picker identity.
pub(crate) struct IconPickerSelection {
    pub(crate) selected_url: Option<String>,
    pub(crate) pending_url: Option<String>,
}

pub(crate) fn icon_picker<T: 'static>(
    props: IconPickerProps<'_>,
    on_intent: IconPickerListener,
    read_selection: impl Fn(&T) -> Option<IconPickerSelection> + 'static,
    window: &Window,
    cx: &Context<T>,
) -> gpui::AnyElement {
    let vm = props.view;
    let theme = theme::get(cx);
    let width = (window.viewport_size().width - px(48.0)).min(px(760.0));
    let height = (window.viewport_size().height - px(48.0)).min(px(600.0));
    let columns = ((f32::from(width) - 56.0) / 96.0).floor().max(1.0) as usize;
    let cell_width = (f32::from(width) - 56.0) / columns as f32;
    let catalog = props.catalog;
    let on_select = on_intent.clone();
    let scroll = props.scroll.0.borrow().base_handle.clone();
    let query = vm.query;
    let matching_icons = vm.matching_icons.clone();
    let match_count = matching_icons.len();

    let grid = uniform_list(
        "server-icon-grid",
        match_count.div_ceil(columns),
        cx.processor(move |owner, rows: std::ops::Range<usize>, _, cx| {
            let Some(selection) = read_selection(owner) else {
                return Vec::new();
            };
            rows.map(|row| {
                div().flex().h(px(96.0)).children(
                    (row * columns..((row + 1) * columns).min(matching_icons.len())).map(
                        |position| {
                            icon_choice(
                                props.session,
                                catalog,
                                matching_icons[position],
                                cell_width,
                                &selection,
                                on_select.clone(),
                                cx,
                            )
                        },
                    ),
                )
            })
            .collect()
        }),
    )
    .size_full()
    .track_scroll(&props.scroll);
    let header = div()
        .flex()
        .items_center()
        .justify_between()
        .child(
            div()
                .text_lg()
                .text_color(theme.foreground)
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child("选择图标"),
        )
        .child(
            div()
                .id("close-server-icon-picker")
                .debug_selector(|| "close-server-icon-picker".into())
                .aria_label("关闭")
                .cursor_pointer()
                .flex()
                .flex_none()
                .size(px(28.0))
                .items_center()
                .justify_center()
                .rounded(radius::CONTROL)
                .hover(move |style| style.bg(theme.secondary_hover))
                .child(
                    svg()
                        .path("icons/window-close.svg")
                        .size(px(18.0))
                        .text_color(theme.foreground),
                )
                .on_click({
                    let on_intent = on_intent.clone();
                    move |_, window, cx| {
                        cx.stop_propagation();
                        on_intent(IconPickerIntent::Close, window, cx);
                    }
                }),
        );
    let panel = div()
        .id("server-icon-picker")
        .debug_selector(|| "server-icon-picker".into())
        .flex()
        .flex_col()
        .w(width)
        .h(height)
        .p_5()
        .gap_3()
        .rounded(radius::SURFACE)
        .border_1()
        .border_color(theme.input_border)
        .bg(theme.dialog_background)
        .shadow_lg()
        .on_click(|_, _, cx| cx.stop_propagation())
        .child(header)
        .child(
            div()
                .id("server-icon-search")
                .debug_selector(|| "server-icon-search".into())
                .flex()
                .flex_none()
                .child(props.search.clone()),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(if query.is_empty() {
                    format!("共 {match_count} 个图标")
                } else {
                    format!("找到 {match_count} 个图标")
                }),
        )
        .child(
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .when(match_count > 0, |this| {
                    this.child(grid).child(Scrollbar::vertical(&scroll))
                })
                .when(match_count == 0, |this| {
                    this.child(
                        div()
                            .id("server-icon-search-empty")
                            .debug_selector(|| "server-icon-search-empty".into())
                            .flex()
                            .size_full()
                            .items_center()
                            .justify_center()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("没有找到匹配的图标"),
                    )
                }),
        )
        .when(vm.pending_url.is_some(), |this| {
            this.child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("正在应用图标…"),
            )
        })
        .when_some(vm.error.map(str::to_owned), |this, error| {
            this.child(div().text_sm().text_color(theme.error).child(error))
        });

    div()
        .id("server-icon-overlay")
        .cursor_default()
        .debug_selector(|| "server-icon-overlay".into())
        .absolute()
        .top_0()
        .right_0()
        .bottom_0()
        .left_0()
        .occlude()
        .track_focus(&props.focus)
        .flex()
        .items_center()
        .justify_center()
        .bg(theme.overlay)
        .rounded_tl(props.corners.top_left)
        .rounded_tr(props.corners.top_right)
        .rounded_bl(props.corners.bottom_left)
        .rounded_br(props.corners.bottom_right)
        .overflow_hidden()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_click({
            let on_intent = on_intent.clone();
            move |_, window, cx| {
                cx.stop_propagation();
                on_intent(IconPickerIntent::Close, window, cx);
            }
        })
        .on_key_down({
            let on_intent = on_intent.clone();
            move |event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    on_intent(IconPickerIntent::Close, window, cx);
                }
            }
        })
        .capture_action(move |_: &Escape, window, cx| {
            cx.stop_propagation();
            on_intent(IconPickerIntent::Close, window, cx);
        })
        .child(panel)
        .into_any_element()
}

fn icon_choice(
    session: Uuid,
    catalog: &[Icon],
    index: usize,
    width: f32,
    selection: &IconPickerSelection,
    on_intent: IconPickerListener,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);
    let icon = &catalog[index];
    let selected = selection.selected_url.as_deref() == Some(icon.url.as_str());
    let pending = selection.pending_url.as_deref() == Some(icon.url.as_str());
    let choice = div()
        .id(("server-icon-choice", index))
        .debug_selector(move || format!("server-icon-choice-{index}"))
        .aria_label(icon.name.clone())
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .size_full()
        .gap_1()
        .px_1()
        .rounded(radius::CONTROL)
        .when(selected, |this| this.bg(theme.element_selected))
        .when(pending, |this| this.opacity(0.5))
        .cursor_default()
        .when(selection.pending_url.is_none(), |this| {
            this.cursor_pointer()
                .hover(move |style| {
                    style.bg(if selected {
                        theme.element_selected_hover
                    } else {
                        theme.secondary_hover
                    })
                })
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    on_intent(IconPickerIntent::Select(index), window, cx);
                })
        })
        .child(icon_preview(session, &icon.url, 48.0))
        .child(
            div()
                .w_full()
                .text_xs()
                .text_center()
                .text_ellipsis()
                .text_color(theme.foreground)
                .child(icon.name.clone()),
        );
    div().w(px(width)).h_full().p_1().child(choice)
}
