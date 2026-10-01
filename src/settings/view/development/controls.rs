//! Development settings input construction and typed selectors.
use super::*;

pub(super) fn number_input(
    placeholder: &'static str,
    value: u64,
    field: NumericSetting,
    cx: &mut Context<PlaybackSettingsDialogState>,
) -> Entity<Editor> {
    let input = cx.new(|cx| {
        Editor::new(placeholder, cx)
            .default_value(value.to_string())
            .borderless()
            .compact()
            .height(px(26.0))
            .centered()
            .digits_only()
            .max_chars(12)
    });
    cx.subscribe(&input, move |this, input, event, cx| {
        if matches!(event, EditorEvent::Changed) {
            this.dispatch(
                SettingsIntent::EditNumber {
                    field,
                    input: input.read(cx).value().to_string(),
                },
                cx,
            );
        }
    })
    .detach();
    input
}

pub(super) fn decimal_input(
    placeholder: &'static str,
    value: f64,
    field: NumericSetting,
    cx: &mut Context<PlaybackSettingsDialogState>,
) -> Entity<Editor> {
    let input = cx.new(|cx| {
        Editor::new(placeholder, cx)
            .default_value(format_seconds(value))
            .borderless()
            .compact()
            .height(px(26.0))
            .centered()
            .max_chars(16)
    });
    cx.subscribe(&input, move |this, input, event, cx| {
        if matches!(event, EditorEvent::Changed) {
            this.dispatch(
                SettingsIntent::EditNumber {
                    field,
                    input: input.read(cx).value().to_string(),
                },
                cx,
            );
        }
    })
    .detach();
    input
}

pub(super) fn mode_selector(
    dialog: Entity<PlaybackSettingsDialogState>,
    selected: PlaybackCacheMode,
    dropdown: Entity<DropdownState>,
) -> impl IntoElement {
    selector_row(
        ("cache-mode-dropdown", "普通缓存"),
        dropdown,
        [
            ("cache-mode-auto", "自动", PlaybackCacheMode::Auto),
            ("cache-mode-enabled", "启用", PlaybackCacheMode::Enabled),
            ("cache-mode-disabled", "关闭", PlaybackCacheMode::Disabled),
        ],
        selected,
        move |mode, cx| dialog.update(cx, |dialog, cx| dialog.select_mode(mode, cx)),
    )
}

pub(super) fn seekable_selector(
    dialog: Entity<PlaybackSettingsDialogState>,
    selected: PlaybackSeekableCacheMode,
    dropdown: Entity<DropdownState>,
) -> impl IntoElement {
    selector_row(
        ("seekable-cache-dropdown", "回看缓存"),
        dropdown,
        [
            (
                "seekable-cache-auto",
                "自动",
                PlaybackSeekableCacheMode::Auto,
            ),
            (
                "seekable-cache-enabled",
                "保留",
                PlaybackSeekableCacheMode::Enabled,
            ),
            (
                "seekable-cache-disabled",
                "关闭",
                PlaybackSeekableCacheMode::Disabled,
            ),
        ],
        selected,
        move |mode, cx| dialog.update(cx, |dialog, cx| dialog.select_seekable_cache(mode, cx)),
    )
}

pub(super) fn unlink_selector(
    dialog: Entity<PlaybackSettingsDialogState>,
    selected: CacheUnlinkPolicy,
    dropdown: Entity<DropdownState>,
) -> impl IntoElement {
    selector_row(
        ("unlink-dropdown", "缓存文件清理"),
        dropdown,
        [
            ("unlink-immediate", "立即删除", CacheUnlinkPolicy::Immediate),
            (
                "unlink-when-done",
                "完成后删除",
                CacheUnlinkPolicy::WhenDone,
            ),
            ("unlink-never", "保留文件", CacheUnlinkPolicy::Never),
        ],
        selected,
        move |policy, cx| dialog.update(cx, |dialog, cx| dialog.select_unlink_files(policy, cx)),
    )
}
