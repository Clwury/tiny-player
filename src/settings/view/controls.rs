//! Settings-specific selectors and layout.
use crate::media::TrackLanguage;
use crate::ui::{
    dropdown::{DropdownState, selector_row},
    editor::{Editor, EditorEvent},
    number_input::{NumberControl, NumberRange},
};
use crate::{
    settings::{AppLanguage, SettingsMode},
    theme,
    ui::radius,
};
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext as _, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, div, px,
};
use tiny_playback::HardwareDecodeMode;

pub(crate) const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;

pub(crate) const USER_HARDWARE_DECODE_DESCRIPTION: &str =
    "自动按平台和显卡选择 GPU 硬解，失败后回退软件解码；强制模式失败时报错。";
pub(crate) const DEVELOPMENT_HARDWARE_DECODE_DESCRIPTION: &str =
    "自动按平台和显卡选择硬解，Linux 下保留 VA-API 回传内存回退；强制模式失败时报错。";
pub(crate) const PLAYBACK_DEBUG_LOG_LABEL: &str = "播放调试日志";
pub(crate) const PLAYBACK_DEBUG_LOG_DESCRIPTION: &str =
    "记录播放 DEBUG 及以上级别的原始消息、状态和错误详情。立即生效。";

#[cfg(test)]
mod tests;

pub(crate) fn playback_log_location(cx: &App) -> impl IntoElement {
    playback_log_path_field(
        crate::logging::playback_log_path(),
        |path, cx| cx.reveal_path(path),
        cx,
    )
}

fn playback_log_path_field(
    path: Option<std::path::PathBuf>,
    on_reveal: impl Fn(&std::path::Path, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let available = path.is_some();
    let value = path
        .as_deref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| crate::i18n::tr("日志目录不可用", cx).to_string());
    let action = crate::i18n::tr("点击在文件管理器中显示并选中文件", cx);
    let tooltip = if available {
        format!("{value}\n{action}")
    } else {
        value.clone()
    };
    let theme = theme::get(cx);
    div()
        .id("playback-debug-log-path")
        .debug_selector(|| "playback-debug-log-path".into())
        .role(if available {
            gpui::Role::Button
        } else {
            gpui::Role::Label
        })
        .aria_label(crate::i18n::tr(format!("日志文件（只读）：{value}"), cx))
        .aria_value(value.clone())
        .aria_description(action)
        .flex()
        .items_center()
        .h(px(32.0))
        .w(px(256.0))
        .px_2()
        .rounded(radius::INPUT)
        .border_1()
        .border_color(theme.window_border)
        .bg(theme.editor_background)
        .cursor_default()
        .when(available, |this| {
            this.cursor_pointer()
                .hover(move |style| style.border_color(theme.accent))
        })
        .text_sm()
        .line_height(gpui::relative(1.3))
        .text_color(theme.muted_foreground)
        .tooltip(move |_, cx| crate::ui::tooltip::text_tooltip(tooltip.clone(), cx))
        .child(div().min_w_0().text_ellipsis().child(value))
        .on_click(move |_, window, cx| {
            if let Some(path) = &path {
                window.blur(cx);
                on_reveal(path, cx);
            }
        })
}

pub(crate) fn app_language_selector(
    state: Entity<DropdownState>,
    selected: AppLanguage,
    on_select: impl Fn(AppLanguage, &mut App) + 'static,
) -> impl IntoElement {
    selector_row(
        ("app-language-dropdown", "应用界面语言"),
        state,
        AppLanguage::ALL.map(|language| (language.id(), language.label(), language)),
        selected,
        on_select,
    )
    .localize_options(false)
}

pub(crate) fn hardware_decode_selector(
    state: Entity<DropdownState>,
    mode: SettingsMode,
    selected: HardwareDecodeMode,
    on_select: impl Fn(HardwareDecodeMode, &mut App) + 'static,
) -> impl IntoElement {
    let mut options = vec![
        ("hardware-decode-auto", "自动", mode.auto_hardware_decode()),
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
        #[cfg(target_os = "linux")]
        (
            "hardware-decode-force-vaapi-vulkan",
            "强制 VA-API（映射 Vulkan）",
            HardwareDecodeMode::ForceVaapiVulkan,
        ),
    ];
    #[cfg(target_os = "linux")]
    if mode == SettingsMode::Development {
        options.insert(
            3,
            (
                "hardware-decode-force-vaapi",
                "强制 VA-API（回传内存）",
                HardwareDecodeMode::ForceVaapi,
            ),
        );
    }
    // Keep preferences from the other settings window visible without changing
    // their policy until the user explicitly selects this window's Auto option.
    if !options.iter().any(|(_, _, value)| *value == selected) {
        let (id, label) = if selected == HardwareDecodeMode::Auto {
            ("hardware-decode-user-auto", "自动（用户设置）")
        } else {
            ("hardware-decode-development", "自定义（开发设置）")
        };
        options.push((id, label, selected));
    }
    selector_row(
        ("hardware-decode-dropdown", "硬件解码"),
        state,
        options,
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
        .aria_label(crate::i18n::tr(title, cx))
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
        .child(crate::i18n::tr(title, cx))
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
