//! Settings-specific selectors and layout.
use crate::media::TrackLanguage;
use crate::ui::{
    dropdown::{DropdownState, selector_row},
    editor::{Editor, EditorEvent},
    number_input::{NumberControl, NumberRange},
};
use crate::{theme, ui::radius};
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext as _, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, div, px,
};
use tiny_playback::HardwareDecodeMode;

pub(crate) const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;

pub(crate) const HARDWARE_DECODE_DESCRIPTION: &str = "自动优先使用硬件解码，失败时回退到软件。";

pub(crate) fn hardware_decode_selector(
    state: Entity<DropdownState>,
    selected: HardwareDecodeMode,
    on_select: impl Fn(HardwareDecodeMode, &mut App) + 'static,
) -> impl IntoElement {
    selector_row(
        ("hardware-decode-dropdown", "硬件解码"),
        state,
        [
            ("hardware-decode-auto", "自动", HardwareDecodeMode::Auto),
            (
                "hardware-decode-off",
                "关闭（软件解码）",
                HardwareDecodeMode::Off,
            ),
            (
                "hardware-decode-force-vulkan",
                "强制 Vulkan",
                HardwareDecodeMode::ForceVulkan,
            ),
        ],
        selected,
        on_select,
    )
}

pub(crate) fn settings_sidebar(
    bottom_left_radius: gpui::Pixels,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::get(cx);
    div()
        .id("settings-sidebar")
        .debug_selector(|| "settings-sidebar".into())
        .flex()
        .flex_col()
        .flex_shrink_0()
        .w(px(226.0))
        .p_2p5()
        .gap_4()
        .border_r_1()
        .border_color(theme.title_bar_border)
        .bg(theme.panel_background)
        .rounded_bl(bottom_left_radius)
        .overflow_hidden()
}

pub(crate) fn settings_category_button(
    title: &'static str,
    selected: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::get(cx);
    div()
        .id(title)
        .role(gpui::Role::Button)
        .aria_label(title)
        .debug_selector(move || format!("settings-category-{title}"))
        .flex()
        .items_center()
        .h(px(28.0))
        .px_2()
        .gap_2()
        .rounded(radius::CONTROL)
        .cursor_pointer()
        .text_sm()
        .text_color(if selected {
            theme.accent_text
        } else {
            theme.muted_foreground
        })
        .when(selected, |this| {
            this.bg(theme.element_selected)
                .font_weight(gpui::FontWeight::MEDIUM)
        })
        .hover(|style| {
            style
                .bg(if selected {
                    theme.element_selected_hover
                } else {
                    theme.secondary_hover
                })
                .text_color(if selected {
                    theme.accent_text
                } else {
                    theme.foreground
                })
        })
        .child(title)
}

pub(crate) fn disk_cache_capacity_input<T: 'static>(
    bytes: u64,
    on_change: impl Fn(&mut T, String, &mut Context<T>) + 'static,
    cx: &mut Context<T>,
) -> Entity<Editor> {
    let input = cx.new(|cx| {
        Editor::new("磁盘缓存上限（GiB）", cx)
            .default_value((bytes / BYTES_PER_GIB).to_string())
            .borderless()
            .compact()
            .height(px(26.0))
            .centered()
            .digits_only()
            .max_chars(12)
    });
    cx.subscribe(&input, move |this, input, event, cx| {
        if matches!(event, EditorEvent::Changed) {
            on_change(this, input.read(cx).value().to_string(), cx);
        }
    })
    .detach();
    input
}

pub(crate) fn disk_cache_capacity_control(input: Entity<Editor>, bytes: u64) -> NumberControl {
    NumberControl::new(
        ("disk-cache", "磁盘缓存上限"),
        input,
        "GiB",
        NumberRange::integer(1, u64::MAX / BYTES_PER_GIB),
        (bytes / BYTES_PER_GIB) as f64,
    )
}

pub(crate) fn track_language_selector(
    header: (&'static str, &'static str),
    state: Entity<DropdownState>,
    selected: TrackLanguage,
    on_select: impl Fn(TrackLanguage, &mut App) + 'static,
) -> impl IntoElement {
    selector_row(
        header,
        state,
        TrackLanguage::ALL.map(|language| (language.id(), language.label(), language)),
        selected,
        on_select,
    )
}
