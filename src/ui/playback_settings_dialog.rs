use std::path::PathBuf;

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px, relative,
};

use crate::{
    player::{
        CacheUnlinkPolicy, PlaybackCacheConfig, PlaybackCacheMode, PlaybackSeekableCacheMode,
    },
    theme,
};

use super::editor::Editor;

const BYTES_PER_MIB: u64 = 1024 * 1024;
const BYTES_PER_GIB: u64 = 1024 * BYTES_PER_MIB;

#[derive(Clone, Copy)]
enum ToggleSetting {
    DiskCache,
    CachePause,
    CachePauseInitial,
    DonateBuffer,
    AdaptiveReadahead,
    AutomaticHysteresis,
    DemuxerCacheWait,
}

pub struct PlaybackSettingsDialogState {
    base_config: PlaybackCacheConfig,
    mode: PlaybackCacheMode,
    seekable_cache: PlaybackSeekableCacheMode,
    unlink_files: CacheUnlinkPolicy,
    cache_dir: Entity<Editor>,
    disk_cache: bool,
    cache_pause: bool,
    cache_pause_initial: bool,
    demuxer_donate_buffer: bool,
    adaptive_readahead: bool,
    automatic_hysteresis: bool,
    demuxer_cache_wait: bool,
    total_cache_mib: Entity<Editor>,
    http_cache_mib: Entity<Editor>,
    http_cache_chunk_mib: Entity<Editor>,
    demuxer_forward_mib: Entity<Editor>,
    demuxer_back_mib: Entity<Editor>,
    range_request_mib: Entity<Editor>,
    cache_secs: Entity<Editor>,
    readahead_secs: Entity<Editor>,
    packet_readahead_secs: Entity<Editor>,
    hysteresis_secs: Entity<Editor>,
    cache_pause_wait_secs: Entity<Editor>,
    max_ranges: Entity<Editor>,
    disk_cache_gib: Entity<Editor>,
}

impl PlaybackSettingsDialogState {
    pub fn new(config: &PlaybackCacheConfig, cx: &mut Context<Self>) -> Self {
        let config = config.clone().normalized();
        Self {
            base_config: config.clone(),
            mode: config.mode,
            seekable_cache: config.seekable_cache,
            unlink_files: config.unlink_files,
            cache_dir: editor_input(
                "默认临时目录",
                config
                    .cache_dir
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                cx,
            ),
            disk_cache: config.disk_cache,
            cache_pause: config.cache_pause,
            cache_pause_initial: config.cache_pause_initial,
            demuxer_donate_buffer: config.demuxer_donate_buffer,
            adaptive_readahead: config.adaptive_readahead,
            automatic_hysteresis: config.automatic_hysteresis,
            demuxer_cache_wait: config.demuxer_cache_wait,
            total_cache_mib: number_input(
                "总缓存上限（MiB，0=独立上限）",
                bytes_to_mib(config.total_cache_max_bytes),
                cx,
            ),
            http_cache_mib: number_input(
                "HTTP 内存缓存（MiB）",
                bytes_to_mib(config.http_cache_max_bytes),
                cx,
            ),
            http_cache_chunk_mib: number_input(
                "HTTP 分页块（MiB）",
                bytes_to_mib(config.http_cache_chunk_bytes),
                cx,
            ),
            demuxer_forward_mib: number_input(
                "Demux 前向缓存（MiB，0=不限）",
                bytes_to_mib(config.demuxer_max_bytes),
                cx,
            ),
            demuxer_back_mib: number_input(
                "Demux 回看缓存（MiB，0=关闭）",
                bytes_to_mib(config.demuxer_max_back_bytes),
                cx,
            ),
            range_request_mib: number_input(
                "HTTP Range 请求（MiB）",
                bytes_to_mib(config.http_cache_range_request_bytes),
                cx,
            ),
            cache_secs: decimal_input("网络缓存目标（秒）", config.cache_secs, cx),
            readahead_secs: decimal_input("Demux 预读（秒）", config.demuxer_readahead_secs, cx),
            packet_readahead_secs: decimal_input(
                "Packet 预读上限（秒，0=不限）",
                config.demuxer_packet_max_readahead_secs,
                cx,
            ),
            hysteresis_secs: decimal_input(
                "滞回带（秒，自动模式下0=自动）",
                config.demuxer_hysteresis_secs,
                cx,
            ),
            cache_pause_wait_secs: decimal_input(
                "Cache-pause 恢复阈值（秒）",
                config.cache_pause_wait,
                cx,
            ),
            max_ranges: number_input("最多保留 Demux range", config.demuxer_max_ranges as u64, cx),
            disk_cache_gib: number_input(
                "磁盘缓存上限（GiB）",
                bytes_to_gib(config.disk_cache_max_bytes),
                cx,
            ),
        }
    }

    pub fn submit(&self, cx: &mut Context<Self>) -> PlaybackCacheConfig {
        let mut config = self.base_config.clone();
        config.mode = self.mode;
        config.seekable_cache = self.seekable_cache;
        config.unlink_files = self.unlink_files;
        config.cache_dir = optional_path(self.cache_dir.read(cx).value().as_ref());
        config.disk_cache = self.disk_cache;
        config.cache_pause = self.cache_pause;
        config.cache_pause_initial = self.cache_pause_initial;
        config.demuxer_donate_buffer = self.demuxer_donate_buffer;
        config.adaptive_readahead = self.adaptive_readahead;
        config.automatic_hysteresis = self.automatic_hysteresis;
        config.demuxer_cache_wait = self.demuxer_cache_wait;
        config.total_cache_max_bytes = mib_to_bytes(
            self.total_cache_mib.read(cx),
            bytes_to_mib(self.base_config.total_cache_max_bytes),
        );
        config.http_cache_max_bytes = mib_to_bytes(
            self.http_cache_mib.read(cx),
            bytes_to_mib(self.base_config.http_cache_max_bytes),
        );
        config.http_cache_chunk_bytes = mib_to_bytes(
            self.http_cache_chunk_mib.read(cx),
            bytes_to_mib(self.base_config.http_cache_chunk_bytes),
        );
        config.demuxer_max_bytes = mib_to_bytes(
            self.demuxer_forward_mib.read(cx),
            bytes_to_mib(self.base_config.demuxer_max_bytes),
        );
        config.demuxer_max_back_bytes = mib_to_bytes(
            self.demuxer_back_mib.read(cx),
            bytes_to_mib(self.base_config.demuxer_max_back_bytes),
        );
        config.http_cache_range_request_bytes = mib_to_bytes(
            self.range_request_mib.read(cx),
            bytes_to_mib(self.base_config.http_cache_range_request_bytes),
        );
        config.cache_secs = seconds_value(self.cache_secs.read(cx), self.base_config.cache_secs);
        config.demuxer_readahead_secs = seconds_value(
            self.readahead_secs.read(cx),
            self.base_config.demuxer_readahead_secs,
        );
        config.demuxer_packet_max_readahead_secs = seconds_value(
            self.packet_readahead_secs.read(cx),
            self.base_config.demuxer_packet_max_readahead_secs,
        );
        config.demuxer_hysteresis_secs = seconds_value(
            self.hysteresis_secs.read(cx),
            self.base_config.demuxer_hysteresis_secs,
        );
        config.cache_pause_wait = seconds_value(
            self.cache_pause_wait_secs.read(cx),
            self.base_config.cache_pause_wait,
        );
        config.demuxer_max_ranges =
            usize::try_from(parse_u64(self.max_ranges.read(cx).value().as_ref(), 10)).unwrap_or(10);
        config.disk_cache_max_bytes = gib_to_bytes(
            self.disk_cache_gib.read(cx),
            bytes_to_gib(self.base_config.disk_cache_max_bytes),
        );
        config.normalized()
    }

    pub fn render_layer(
        &self,
        dialog: Entity<Self>,
        rounded_window: bool,
        on_cancel: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        on_submit: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &App,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.overlay)
            // This layer is a modal hitbox. Without an occluding hitbox the
            // transparent areas around the panel have no target of their own,
            // so GPUI continues dispatching clicks and wheel events to the
            // page rendered underneath it.
            .occlude()
            .when(rounded_window, |this| {
                this.rounded(theme.radius_lg).overflow_hidden()
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(700.0))
                    .max_h(relative(0.92))
                    .gap_4()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.input_border)
                    .bg(theme.dialog_background)
                    .p_5()
                    .shadow_lg()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child("播放缓存设置"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("设置会保存到本地，并在下一次播放及后续播放会话中生效。"),
                    )
                    .child(
                        div()
                            .id("playback-settings-scroll")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(self.render_form(dialog.clone(), cx)),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                dialog_button("cancel-playback-settings", "取消", false, cx)
                                    .on_click(on_cancel),
                            )
                            .child(
                                dialog_button("submit-playback-settings", "保存", true, cx)
                                    .on_click(on_submit),
                            ),
                    ),
            )
    }

    fn render_form(&self, dialog: Entity<Self>, cx: &App) -> impl IntoElement {
        let theme = theme::get(cx);
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(section_title("缓存模式", cx))
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(div().flex_1().child(field(
                        "普通缓存",
                        mode_selector(dialog.clone(), self.mode, cx),
                        cx,
                    )))
                    .child(div().flex_1().child(field(
                        "可 seek 缓存",
                        seekable_selector(dialog.clone(), self.seekable_cache, cx),
                        cx,
                    ))),
            )
            .child(section_title("内存与 Range", cx))
            .child(two_columns(
                field("总缓存上限（MiB，0=独立上限）", self.total_cache_mib.clone(), cx),
                field("HTTP 内存缓存（MiB）", self.http_cache_mib.clone(), cx),
            ))
            .child(two_columns(
                field("HTTP 分页块（MiB）", self.http_cache_chunk_mib.clone(), cx),
                field("HTTP Range 请求（MiB）", self.range_request_mib.clone(), cx),
            ))
            .child(two_columns(
                field(
                    "Demux 前向缓存（MiB，0=共享预算内不限）",
                    self.demuxer_forward_mib.clone(),
                    cx,
                ),
                field(
                    "Demux 回看缓存（MiB，0=关闭）",
                    self.demuxer_back_mib.clone(),
                    cx,
                ),
            ))
            .child(two_columns(
                field("最多保留 Demux range", self.max_ranges.clone(), cx),
                field("磁盘缓存上限（GiB）", self.disk_cache_gib.clone(), cx),
            ))
            .child(section_title("磁盘缓存", cx))
            .child(field(
                "缓存目录（留空=默认临时目录）",
                self.cache_dir.clone(),
                cx,
            ))
            .child(field(
                "缓存文件清理",
                unlink_selector(dialog.clone(), self.unlink_files, cx),
                cx,
            ))
            .child(section_title("预读与暂停", cx))
            .child(two_columns(
                field("网络缓存目标（秒）", self.cache_secs.clone(), cx),
                field("Demux 预读（秒）", self.readahead_secs.clone(), cx),
            ))
            .child(two_columns(
                field(
                    "Packet 预读上限（秒，0=不限）",
                    self.packet_readahead_secs.clone(),
                    cx,
                ),
                field(
                    "滞回带（秒，自动模式下0=自动）",
                    self.hysteresis_secs.clone(),
                    cx,
                ),
            ))
            .child(field(
                "Cache-pause 恢复阈值（秒）",
                self.cache_pause_wait_secs.clone(),
                cx,
            ))
            .child(section_title("策略开关", cx))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children([
                        toggle_button(
                            dialog.clone(),
                            ToggleSetting::DiskCache,
                            "启用磁盘缓存",
                            self.disk_cache,
                            cx,
                        ),
                        toggle_button(
                            dialog.clone(),
                            ToggleSetting::CachePause,
                            "启用 cache-pause",
                            self.cache_pause,
                            cx,
                        ),
                        toggle_button(
                            dialog.clone(),
                            ToggleSetting::CachePauseInitial,
                            "启动时等待缓存",
                            self.cache_pause_initial,
                            cx,
                        ),
                        toggle_button(
                            dialog.clone(),
                            ToggleSetting::DonateBuffer,
                            "回看预算可捐献",
                            self.demuxer_donate_buffer,
                            cx,
                        ),
                        toggle_button(
                            dialog.clone(),
                            ToggleSetting::AdaptiveReadahead,
                            "自适应预读",
                            self.adaptive_readahead,
                            cx,
                        ),
                        toggle_button(
                            dialog.clone(),
                            ToggleSetting::AutomaticHysteresis,
                            "自动滞回",
                            self.automatic_hysteresis,
                            cx,
                        ),
                        toggle_button(
                            dialog,
                            ToggleSetting::DemuxerCacheWait,
                            "Demux 缓存等待",
                            self.demuxer_cache_wait,
                            cx,
                        ),
                    ]),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("提示：总缓存上限会按 HTTP → Demux 前向 → 回看顺序分配；总额为 0 时恢复独立上限，前向为 0 时在总额内尽可能使用。"),
            )
    }

    fn select_mode(&mut self, mode: PlaybackCacheMode, cx: &mut Context<Self>) {
        self.mode = mode;
        cx.notify();
    }

    fn select_seekable_cache(&mut self, mode: PlaybackSeekableCacheMode, cx: &mut Context<Self>) {
        self.seekable_cache = mode;
        cx.notify();
    }

    fn select_unlink_files(&mut self, policy: CacheUnlinkPolicy, cx: &mut Context<Self>) {
        self.unlink_files = policy;
        cx.notify();
    }

    fn toggle(&mut self, setting: ToggleSetting, cx: &mut Context<Self>) {
        match setting {
            ToggleSetting::DiskCache => self.disk_cache = !self.disk_cache,
            ToggleSetting::CachePause => self.cache_pause = !self.cache_pause,
            ToggleSetting::CachePauseInitial => {
                self.cache_pause_initial = !self.cache_pause_initial
            }
            ToggleSetting::DonateBuffer => self.demuxer_donate_buffer = !self.demuxer_donate_buffer,
            ToggleSetting::AdaptiveReadahead => self.adaptive_readahead = !self.adaptive_readahead,
            ToggleSetting::AutomaticHysteresis => {
                self.automatic_hysteresis = !self.automatic_hysteresis
            }
            ToggleSetting::DemuxerCacheWait => self.demuxer_cache_wait = !self.demuxer_cache_wait,
        }
        cx.notify();
    }
}

fn number_input(
    placeholder: &'static str,
    value: u64,
    cx: &mut Context<PlaybackSettingsDialogState>,
) -> Entity<Editor> {
    cx.new(|cx| {
        Editor::new(placeholder, cx)
            .default_value(value.to_string())
            .digits_only()
            .max_chars(12)
    })
}

fn editor_input(
    placeholder: &'static str,
    value: String,
    cx: &mut Context<PlaybackSettingsDialogState>,
) -> Entity<Editor> {
    cx.new(|cx| {
        Editor::new(placeholder, cx)
            .default_value(value)
            .max_chars(256)
    })
}

fn decimal_input(
    placeholder: &'static str,
    value: f64,
    cx: &mut Context<PlaybackSettingsDialogState>,
) -> Entity<Editor> {
    cx.new(|cx| {
        Editor::new(placeholder, cx)
            .default_value(format_seconds(value))
            .max_chars(16)
    })
}

fn bytes_to_mib(bytes: u64) -> u64 {
    bytes / BYTES_PER_MIB
}

fn bytes_to_gib(bytes: u64) -> u64 {
    bytes / BYTES_PER_GIB
}

fn parse_u64(value: &str, fallback: u64) -> u64 {
    value.trim().parse::<u64>().unwrap_or(fallback)
}

fn mib_to_bytes(input: &Editor, fallback_mib: u64) -> u64 {
    parse_u64(input.value().as_ref(), fallback_mib).saturating_mul(BYTES_PER_MIB)
}

fn gib_to_bytes(input: &Editor, fallback_gib: u64) -> u64 {
    parse_u64(input.value().as_ref(), fallback_gib).saturating_mul(BYTES_PER_GIB)
}

fn seconds_value(input: &Editor, fallback: f64) -> f64 {
    let fallback = if fallback.is_finite() && fallback >= 0.0 {
        fallback
    } else {
        0.0
    };
    input
        .value()
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .unwrap_or(fallback)
}

fn format_seconds(value: f64) -> String {
    let value = if value.is_finite() && value >= 0.0 {
        value
    } else {
        0.0
    };
    let mut formatted = format!("{value:.3}");
    while formatted.ends_with('0') {
        formatted.pop();
    }
    if formatted.ends_with('.') {
        formatted.pop();
    }
    if formatted.is_empty() {
        "0".to_string()
    } else {
        formatted
    }
}

fn optional_path(value: &str) -> Option<PathBuf> {
    let value = value.trim();
    (!value.is_empty()).then(|| PathBuf::from(value))
}

fn field(label: &'static str, input: impl IntoElement, cx: &App) -> impl IntoElement {
    let theme = theme::get(cx);
    div()
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(label),
        )
        .child(input)
}

fn section_title(label: &'static str, cx: &App) -> impl IntoElement {
    let theme = theme::get(cx);
    div()
        .text_sm()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.foreground)
        .child(label)
}

fn two_columns(left: impl IntoElement, right: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .gap_3()
        .child(div().flex_1().child(left))
        .child(div().flex_1().child(right))
}

fn mode_selector(
    dialog: Entity<PlaybackSettingsDialogState>,
    selected: PlaybackCacheMode,
    cx: &App,
) -> impl IntoElement {
    selector_row(
        [
            ("cache-mode-auto", "自动", PlaybackCacheMode::Auto),
            ("cache-mode-enabled", "启用", PlaybackCacheMode::Enabled),
            ("cache-mode-disabled", "关闭", PlaybackCacheMode::Disabled),
        ],
        selected,
        move |mode, cx| dialog.update(cx, |dialog, cx| dialog.select_mode(mode, cx)),
        cx,
    )
}

fn seekable_selector(
    dialog: Entity<PlaybackSettingsDialogState>,
    selected: PlaybackSeekableCacheMode,
    cx: &App,
) -> impl IntoElement {
    let theme = theme::get(cx);
    div()
        .flex()
        .h(px(34.0))
        .w_full()
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.input_border)
        .bg(theme.input_background)
        .p(px(2.0))
        .gap_0p5()
        .children([
            selector_button(
                "seekable-cache-auto",
                "自动",
                selected == PlaybackSeekableCacheMode::Auto,
                {
                    let dialog = dialog.clone();
                    move |_, _, cx| {
                        dialog.update(cx, |dialog, cx| {
                            dialog.select_seekable_cache(PlaybackSeekableCacheMode::Auto, cx)
                        });
                    }
                },
                cx,
            ),
            selector_button(
                "seekable-cache-enabled",
                "保留",
                selected == PlaybackSeekableCacheMode::Enabled,
                {
                    let dialog = dialog.clone();
                    move |_, _, cx| {
                        dialog.update(cx, |dialog, cx| {
                            dialog.select_seekable_cache(PlaybackSeekableCacheMode::Enabled, cx)
                        });
                    }
                },
                cx,
            ),
            selector_button(
                "seekable-cache-disabled",
                "关闭",
                selected == PlaybackSeekableCacheMode::Disabled,
                move |_, _, cx| {
                    dialog.update(cx, |dialog, cx| {
                        dialog.select_seekable_cache(PlaybackSeekableCacheMode::Disabled, cx)
                    });
                },
                cx,
            ),
        ])
}

fn unlink_selector(
    dialog: Entity<PlaybackSettingsDialogState>,
    selected: CacheUnlinkPolicy,
    cx: &App,
) -> impl IntoElement {
    selector_row(
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
        cx,
    )
}

fn selector_row<T: Copy + PartialEq + 'static>(
    options: [(&'static str, &'static str, T); 3],
    selected: T,
    on_select: impl Fn(T, &mut gpui::App) + Clone + 'static,
    cx: &App,
) -> impl IntoElement {
    div()
        .flex()
        .h(px(34.0))
        .w_full()
        .rounded(px(8.0))
        .border_1()
        .border_color(theme::get(cx).input_border)
        .bg(theme::get(cx).input_background)
        .p(px(2.0))
        .gap_0p5()
        .children(options.into_iter().map(|(id, label, value)| {
            let on_select = on_select.clone();
            selector_button(
                id,
                label,
                selected == value,
                move |_, _, cx| on_select(value, cx),
                cx,
            )
        }))
}

fn selector_button(
    id: &'static str,
    label: &'static str,
    selected: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::get(cx);
    div()
        .id(id)
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .text_xs()
        .text_color(theme.foreground)
        .when(selected, |this| this.bg(theme.secondary_hover))
        .hover(move |style| style.bg(theme.secondary_hover))
        .child(label)
        .on_click(on_click)
}

fn toggle_button(
    dialog: Entity<PlaybackSettingsDialogState>,
    setting: ToggleSetting,
    label: &'static str,
    selected: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::get(cx);
    div()
        .id(label)
        .flex()
        .items_center()
        .h(px(30.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(if selected {
            theme.input_border_focused
        } else {
            theme.input_border
        })
        .bg(if selected {
            theme.secondary_hover
        } else {
            theme.input_background
        })
        .px_3()
        .text_xs()
        .text_color(theme.foreground)
        .child(if selected {
            format!("✓ {label}")
        } else {
            label.to_string()
        })
        .on_click(move |_, _, cx| {
            dialog.update(cx, |dialog, cx| dialog.toggle(setting, cx));
        })
}

fn dialog_button(
    id: &'static str,
    label: &'static str,
    primary: bool,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = theme::get(cx);
    div()
        .id(id)
        .flex()
        .h(px(34.0))
        .items_center()
        .justify_center()
        .rounded(px(8.0))
        .px_4()
        .text_sm()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(if primary {
            theme.background
        } else {
            theme.foreground
        })
        .border_1()
        .border_color(if primary {
            theme.input_border_focused
        } else {
            theme.input_border
        })
        .bg(if primary {
            theme.foreground
        } else {
            theme.input_background
        })
        .hover(move |style| {
            if primary {
                style.bg(theme.foreground).text_color(theme.background)
            } else {
                style.bg(theme.secondary_hover).text_color(theme.foreground)
            }
        })
        .child(label)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        BYTES_PER_GIB, BYTES_PER_MIB, bytes_to_gib, bytes_to_mib, format_seconds, optional_path,
        parse_u64,
    };

    #[test]
    fn byte_units_round_down_for_display() {
        assert_eq!(bytes_to_mib(3 * BYTES_PER_MIB + 1), 3);
        assert_eq!(bytes_to_gib(2 * BYTES_PER_GIB + 1), 2);
    }

    #[test]
    fn invalid_numeric_values_use_fallbacks() {
        assert_eq!(parse_u64("", 7), 7);
        assert_eq!(parse_u64("abc", 9), 9);
        assert_eq!(parse_u64("12", 9), 12);
    }

    #[test]
    fn seconds_are_displayed_without_unnecessary_zeroes() {
        assert_eq!(format_seconds(2.5), "2.5");
        assert_eq!(format_seconds(2.0), "2");
        assert_eq!(format_seconds(f64::NAN), "0");
    }

    #[test]
    fn empty_cache_directory_is_treated_as_default() {
        assert_eq!(optional_path("  "), None);
        assert_eq!(
            optional_path("/tmp/tiny-player"),
            Some(Path::new("/tmp/tiny-player").into())
        );
    }
}
