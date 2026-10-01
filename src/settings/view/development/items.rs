//! Ordered development setting groups; no duplicate settings state.
use super::controls::{mode_selector, seekable_selector, unlink_selector};
use super::layout::SettingItem;
use super::*;
mod cache;
mod playback;

impl PlaybackSettingsDialogState {
    pub(super) fn setting_items(&self, dialog: Entity<Self>, cx: &App) -> Vec<SettingItem> {
        self.playback_items(dialog.clone(), cx)
            .into_iter()
            .chain(self.cache_items(dialog, cx))
            .collect()
    }

    fn render_cache_directories(&self, cx: &App) -> impl IntoElement {
        let theme = theme::get(cx);
        let separate_directories = self.cache_directories[0] != self.cache_directories[1];
        div().flex().flex_col().w(px(256.0)).gap_2().children(
            self.cache_directories
                .iter()
                .enumerate()
                .filter(|(index, _)| *index == 0 || separate_directories)
                .map(|(index, path)| {
                    let path = path.to_string_lossy().into_owned();
                    let tooltip = path.clone();
                    let label = if separate_directories {
                        ["HTTP 缓存目录", "Demux 缓存目录"][index]
                    } else {
                        "缓存目录"
                    };
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .when(separate_directories, |this| {
                            this.child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(label),
                            )
                        })
                        .child(
                            div()
                                .id(("cache-directory", index))
                                .debug_selector(move || format!("cache-directory-{index}"))
                                .role(gpui::Role::Label)
                                .aria_label(format!("{label}（只读）：{path}"))
                                .flex()
                                .items_center()
                                .h(px(32.0))
                                .px_2()
                                .rounded(radius::INPUT)
                                .border_1()
                                .border_color(theme.window_border)
                                .bg(theme.editor_background)
                                .cursor_default()
                                .text_sm()
                                .line_height(gpui::relative(1.3))
                                .text_color(theme.muted_foreground)
                                .tooltip(move |_, cx| text_tooltip(tooltip.clone(), cx))
                                .child(div().min_w_0().text_ellipsis().child(path)),
                        )
                }),
        )
    }
}

fn toggle(
    dialog: &Entity<PlaybackSettingsDialogState>,
    cx: &App,
    setting: ToggleSetting,
    label: &'static str,
    selected: bool,
) -> impl IntoElement {
    let dialog = dialog.clone();
    toggle_switch(
        label,
        selected,
        move |cx| dialog.update(cx, |dialog, cx| dialog.toggle(setting, cx)),
        cx,
    )
}
