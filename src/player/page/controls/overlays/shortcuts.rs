use super::*;
use crate::ui::radius;
use gpui::App;

pub(super) const SHORTCUT_HELP: &[(&[&str], &str)] = &[
    (&["Space", "P"], "播放 / 暂停"),
    (&["F"], "切换全屏"),
    (&["Esc"], "关闭选集列表；退出全屏或返回详情页"),
    (&["←", "→"], "后退 / 前进 5 秒"),
    (&["↓", "↑"], "后退 / 前进 60 秒"),
    (&["<", ">"], "播放列表上一集 / 下一集"),
    (&["Enter"], "播放列表下一集"),
    (&["F8"], "显示 / 隐藏播放列表"),
    (&["I"], "显示 / 隐藏播放信息"),
    (&["j"], "下一条字幕轨道（含关闭）"),
    (&["Shift-J"], "上一条字幕轨道（含关闭）"),
    (&["v"], "显示 / 隐藏字幕"),
    (&["#"], "切换音轨（含关闭）"),
    (&["R", "T"], "上移 / 下移字幕"),
    (&["9", "/"], "音量降低 2 个百分点"),
    (&["0", "*"], "音量提高 2 个百分点"),
    (&["M"], "静音 / 取消静音"),
    (&["["], "播放速度除以 1.1"),
    (&["]"], "播放速度乘以 1.1"),
    (&["{", "}"], "播放速度减半 / 加倍"),
    (&["Backspace"], "恢复 1 倍速"),
];

pub(super) const GAMEPAD_HELP: &[(&[&str], &str)] = &[
    (&["A"], "播放 / 暂停"),
    (&["B"], "关闭菜单或说明面板；否则返回详情页"),
    (&["←", "→"], "后退 / 前进 5 秒"),
    (&["↓", "↑"], "音量降低 / 提高 2 个百分点"),
    (&["LB", "RB"], "后退 / 前进 60 秒"),
    (&["X"], "静音 / 取消静音"),
    (&["Y"], "显示 / 隐藏播放信息"),
    (&["Menu", "Start"], "切换全屏"),
    (&["View", "Select"], "显示 / 隐藏播放控制栏"),
    (&["LT", "RT"], "降低 / 提高倍速"),
    (&["L3"], "恢复 1 倍速"),
    (&["R3-↑", "R3-↓"], "按住 R3，上移 / 下移字幕"),
];

const GAMEPAD_HINT: &str = "L3 / R3 为左 / 右摇杆按下，箭头为方向键。";

pub(super) fn shortcuts_overlay(
    gamepad_first: bool,
    window: &Window,
    cx: &App,
) -> impl IntoElement {
    let keyboard = ("playback-shortcut", "键盘", SHORTCUT_HELP, None);
    let gamepad = (
        "playback-gamepad",
        "手柄（Deck / Xbox 布局）",
        GAMEPAD_HELP,
        Some(GAMEPAD_HINT),
    );
    let sections = if gamepad_first {
        [gamepad, keyboard]
    } else {
        [keyboard, gamepad]
    };
    playback_overlay_container("playback-shortcuts-overlay", window)
        .gap_3()
        .child(
            div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(outlined_text(crate::i18n::tr("快捷键", cx))),
        )
        .children(sections.into_iter().map(|(id, title, rows, hint)| {
            shortcut_help_section(id, title, rows, hint, window, cx)
        }))
}

fn shortcut_help_section(
    id: &'static str,
    title: &'static str,
    rows: &'static [(&[&str], &str)],
    hint: Option<&'static str>,
    window: &Window,
    cx: &App,
) -> gpui::Div {
    let rows_per_column = if window.viewport_size().width >= px(900.0) {
        rows.len().div_ceil(2)
    } else {
        rows.len()
    };
    div()
        .debug_selector(move || format!("{id}-section"))
        .flex()
        .flex_col()
        .flex_none()
        .gap_2()
        .child(
            div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(outlined_text(crate::i18n::tr(title, cx))),
        )
        .when_some(hint, |this, hint| {
            this.child(
                div()
                    .text_xs()
                    .child(outlined_text(crate::i18n::tr(hint, cx))),
            )
        })
        .child(
            div()
                .flex()
                .gap_6()
                .children(rows.chunks(rows_per_column).map(|rows| {
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .children(rows.iter().map(|(keys, description)| {
                            div()
                                .debug_selector(move || format!("{id}-row-{description}"))
                                .flex()
                                .items_start()
                                .gap_3()
                                .child(shortcut_key_tags(id, keys, cx))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .py(px(2.0))
                                        .child(outlined_text(crate::i18n::tr(*description, cx))),
                                )
                        }))
                })),
        )
}

fn shortcut_key_tags(id: &'static str, keys: &'static [&'static str], cx: &App) -> gpui::Div {
    keys.iter().enumerate().fold(
        div().w(px(144.0)).flex_none().flex().items_center().gap_1(),
        |tags, (index, key)| {
            tags.when(index > 0, |tags| {
                tags.child(
                    div()
                        .debug_selector(move || format!("{id}-separator-{}-{index}", keys[0]))
                        .text_xs()
                        .text_color(theme::media_overlay(cx).muted_foreground)
                        .child(outlined_text("/".into())),
                )
            })
            .child(shortcut_key_tag(id, key, cx))
        },
    )
}

fn shortcut_key_tag(id: &'static str, key: &'static str, cx: &App) -> gpui::Div {
    let theme = theme::get(cx);
    div()
        .debug_selector(move || format!("{id}-key-{key}"))
        .flex_none()
        .px_2()
        .py(px(1.0))
        .rounded(radius::CONTROL)
        .border_1()
        .border_color(theme.accent.opacity(0.32))
        .bg(theme.accent.opacity(0.20))
        .text_color(theme.accent_text)
        .text_xs()
        .italic()
        .child(key)
}

fn outlined_text(text: SharedString) -> gpui::Div {
    div()
        .relative()
        .children(
            [(-1.0, 0.0), (1.0, 0.0), (0.0, -1.0), (0.0, 1.0)].map(|(x, y)| {
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w_full()
                    .text_color(rgb(0x000000))
                    .child(text.clone())
            }),
        )
        .child(text)
}
