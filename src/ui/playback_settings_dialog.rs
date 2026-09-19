//! Full settings for development and playback diagnostics.

use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollHandle, StatefulInteractiveElement, Styled, Subscription, Window,
    div, point, prelude::FluentBuilder, px, relative, svg,
};

use crate::{
    app::window_has_rounded_corners,
    app_metadata::default_playback_cache_dir,
    player::{
        CacheUnlinkPolicy, PlaybackCacheConfig, PlaybackCacheMode, PlaybackLanguagePreferences,
        PlaybackSeekableCacheMode, TrackLanguage,
    },
    theme::{self, ColorTheme},
};

use super::{
    editor::{Editor, EditorEvent},
    scrollbar::Scrollbar,
    settings_controls::{
        DropdownState, NumberControl, NumberRange, disk_cache_capacity_control,
        disk_cache_capacity_input, selector_row, settings_category_button, settings_sidebar,
        toggle_switch, track_language_selector,
    },
    settings_dialog::SettingsChanged,
    tooltip::text_tooltip,
};

const BYTES_PER_MIB: u64 = 1024 * 1024;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum SettingsCategory {
    #[default]
    Appearance,
    Playback,
    General,
    Memory,
    Disk,
    Readahead,
}

impl SettingsCategory {
    const ALL: [Self; 6] = [
        Self::Appearance,
        Self::Playback,
        Self::General,
        Self::Memory,
        Self::Disk,
        Self::Readahead,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Appearance => "外观",
            Self::Playback => "播放",
            Self::General => "缓存策略",
            Self::Memory => "内存缓存",
            Self::Disk => "磁盘缓存",
            Self::Readahead => "预读策略",
        }
    }
}

struct SettingItem {
    category: SettingsCategory,
    section: &'static str,
    title: &'static str,
    description: &'static str,
    keywords: &'static str,
    control: AnyElement,
}

impl SettingItem {
    fn new(
        category: SettingsCategory,
        section: &'static str,
        title: &'static str,
        description: &'static str,
        keywords: &'static str,
        control: impl IntoElement,
    ) -> Self {
        Self {
            category,
            section,
            title,
            description,
            keywords,
            control: control.into_any_element(),
        }
    }

    fn matches_query(&self, query: &str) -> bool {
        matches_search(
            query,
            &[
                self.category.title(),
                self.section,
                self.title,
                self.description,
                self.keywords,
            ],
        )
    }

    fn render(self, last_in_section: bool, cx: &App) -> impl IntoElement {
        let theme = theme::get(cx);
        div()
            .id(self.title)
            .debug_selector(move || self.title.into())
            .flex()
            .min_w_0()
            .w_full()
            // Zed SettingsPageItem::render: 16px between rows, 40px after a section.
            .pt_4()
            .when_else(
                last_in_section,
                |this| this.pb_10(),
                |this| {
                    this.pb_4()
                        .border_b_1()
                        .border_color(theme.title_bar_border)
                },
            )
            .gap_4()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .max_w(relative(2.0 / 3.0))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.foreground)
                            .child(self.title),
                    )
                    .child(
                        div()
                            .text_xs()
                            .line_height(relative(1.5))
                            .text_color(theme.muted_foreground)
                            .child(self.description),
                    ),
            )
            .child(div().flex_shrink_0().child(self.control))
    }
}

#[derive(Clone, Copy)]
enum ToggleSetting {
    DecoderFramedrop,
    DiskCache,
    CachePause,
    CachePauseInitial,
    DonateBuffer,
    AdaptiveReadahead,
    AutomaticHysteresis,
    DemuxerCacheWait,
}

pub struct PlaybackSettingsDialogState {
    category: SettingsCategory,
    color_theme: ColorTheme,
    track_languages: PlaybackLanguagePreferences,
    search: Entity<Editor>,
    dropdown: Entity<DropdownState>,
    scroll_handle: ScrollHandle,
    _search_subscription: Subscription,
    base_config: PlaybackCacheConfig,
    mode: PlaybackCacheMode,
    seekable_cache: PlaybackSeekableCacheMode,
    unlink_files: CacheUnlinkPolicy,
    cache_directories: [PathBuf; 2],
    disk_cache: bool,
    cache_pause: bool,
    cache_pause_initial: bool,
    demuxer_donate_buffer: bool,
    adaptive_readahead: bool,
    automatic_hysteresis: bool,
    demuxer_cache_wait: bool,
    decoder_framedrop: bool,
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

impl EventEmitter<SettingsChanged> for PlaybackSettingsDialogState {}

impl Render for PlaybackSettingsDialogState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rounded_window = window_has_rounded_corners(window);
        self.render_content(cx.entity(), rounded_window, cx)
    }
}

impl PlaybackSettingsDialogState {
    pub fn new(config: &PlaybackCacheConfig, cx: &mut Context<Self>) -> Self {
        let config = config.clone().normalized();
        let search = cx.new(|cx| {
            Editor::new("搜索设置…", cx)
                .compact()
                .borderless()
                .clearable()
        });
        let scroll_handle = ScrollHandle::new();
        let search_subscription = cx.subscribe(&search, |this, _, event, cx| {
            if matches!(event, EditorEvent::Changed) {
                this.dropdown.update(cx, |state, cx| state.close(cx));
                this.scroll_handle.set_offset(point(px(0.0), px(0.0)));
                cx.notify();
            }
        });
        Self {
            category: SettingsCategory::default(),
            color_theme: theme::get(cx).selection,
            track_languages: PlaybackLanguagePreferences::get(cx),
            search,
            dropdown: cx.new(DropdownState::new),
            scroll_handle,
            _search_subscription: search_subscription,
            base_config: config.clone(),
            mode: config.mode,
            seekable_cache: config.seekable_cache,
            unlink_files: config.unlink_files,
            cache_directories: resolved_cache_directories(
                config.cache_dir.as_deref(),
                ["TINY_HTTP_CACHE_DIR", "TINY_DEMUX_PACKET_CACHE_DIR"]
                    .map(|key| std::env::var(key).ok().map(PathBuf::from)),
            ),
            disk_cache: config.disk_cache,
            cache_pause: config.cache_pause,
            cache_pause_initial: config.cache_pause_initial,
            demuxer_donate_buffer: config.demuxer_donate_buffer,
            adaptive_readahead: config.adaptive_readahead,
            automatic_hysteresis: config.automatic_hysteresis,
            demuxer_cache_wait: config.demuxer_cache_wait,
            decoder_framedrop: config.decoder_framedrop,
            total_cache_mib: number_input(
                "总缓存上限（MiB，0=独立上限）",
                bytes_to_mib(config.total_cache_max_bytes),
                |config, value| config.total_cache_max_bytes = value.saturating_mul(BYTES_PER_MIB),
                cx,
            ),
            http_cache_mib: number_input(
                "HTTP 内存缓存（MiB）",
                bytes_to_mib(config.http_cache_max_bytes),
                |config, value| config.http_cache_max_bytes = value.saturating_mul(BYTES_PER_MIB),
                cx,
            ),
            http_cache_chunk_mib: number_input(
                "HTTP 分页块（MiB）",
                bytes_to_mib(config.http_cache_chunk_bytes),
                |config, value| config.http_cache_chunk_bytes = value.saturating_mul(BYTES_PER_MIB),
                cx,
            ),
            demuxer_forward_mib: number_input(
                "Demux 前向缓存（MiB，0=不限）",
                bytes_to_mib(config.demuxer_max_bytes),
                |config, value| config.demuxer_max_bytes = value.saturating_mul(BYTES_PER_MIB),
                cx,
            ),
            demuxer_back_mib: number_input(
                "Demux 回看缓存（MiB，0=关闭）",
                bytes_to_mib(config.demuxer_max_back_bytes),
                |config, value| config.demuxer_max_back_bytes = value.saturating_mul(BYTES_PER_MIB),
                cx,
            ),
            range_request_mib: number_input(
                "HTTP Range 请求（MiB）",
                bytes_to_mib(config.http_cache_range_request_bytes),
                |config, value| {
                    config.http_cache_range_request_bytes = value.saturating_mul(BYTES_PER_MIB)
                },
                cx,
            ),
            cache_secs: decimal_input(
                "网络缓存目标（秒）",
                config.cache_secs,
                |config, value| config.cache_secs = value,
                cx,
            ),
            readahead_secs: decimal_input(
                "Demux 预读（秒）",
                config.demuxer_readahead_secs,
                |config, value| config.demuxer_readahead_secs = value,
                cx,
            ),
            packet_readahead_secs: decimal_input(
                "Packet 预读上限（秒，0=不限）",
                config.demuxer_packet_max_readahead_secs,
                |config, value| config.demuxer_packet_max_readahead_secs = value,
                cx,
            ),
            hysteresis_secs: decimal_input(
                "滞回带（秒，自动模式下0=自动）",
                config.demuxer_hysteresis_secs,
                |config, value| config.demuxer_hysteresis_secs = value,
                cx,
            ),
            cache_pause_wait_secs: decimal_input(
                "Cache-pause 恢复阈值（秒）",
                config.cache_pause_wait,
                |config, value| config.cache_pause_wait = value,
                cx,
            ),
            max_ranges: number_input(
                "最多保留 Demux range",
                config.demuxer_max_ranges as u64,
                |config, value| {
                    config.demuxer_max_ranges = usize::try_from(value).unwrap_or(usize::MAX)
                },
                cx,
            ),
            disk_cache_gib: disk_cache_capacity_input(
                config.disk_cache_max_bytes,
                |this, bytes, cx| {
                    if this.base_config.disk_cache_max_bytes != bytes {
                        this.base_config.disk_cache_max_bytes = bytes;
                        this.changed(cx);
                    }
                },
                cx,
            ),
        }
    }

    pub fn color_theme(&self) -> ColorTheme {
        self.color_theme
    }

    pub fn track_languages(&self) -> PlaybackLanguagePreferences {
        self.track_languages
    }

    fn select_track_language(
        &mut self,
        language: TrackLanguage,
        audio: bool,
        cx: &mut Context<Self>,
    ) {
        let selected = if audio {
            &mut self.track_languages.audio
        } else {
            &mut self.track_languages.subtitle
        };
        if *selected == language {
            return;
        }
        *selected = language;
        self.track_languages.apply(cx);
        self.changed(cx);
    }

    fn select_color_theme(&mut self, selection: ColorTheme, cx: &mut Context<Self>) {
        if self.color_theme == selection {
            return;
        }
        theme::set(selection, cx);
        self.color_theme = theme::get(cx).selection;
        self.changed(cx);
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.base_config = self.playback_config();
        cx.emit(SettingsChanged);
        cx.notify();
    }

    pub fn playback_config(&self) -> PlaybackCacheConfig {
        let mut config = self.base_config.clone();
        config.mode = self.mode;
        config.seekable_cache = self.seekable_cache;
        config.unlink_files = self.unlink_files;
        config.disk_cache = self.disk_cache;
        config.cache_pause = self.cache_pause;
        config.cache_pause_initial = self.cache_pause_initial;
        config.demuxer_donate_buffer = self.demuxer_donate_buffer;
        config.adaptive_readahead = self.adaptive_readahead;
        config.automatic_hysteresis = self.automatic_hysteresis;
        config.demuxer_cache_wait = self.demuxer_cache_wait;
        config.decoder_framedrop = self.decoder_framedrop;
        config.normalized()
    }

    fn render_content(
        &self,
        dialog: Entity<Self>,
        rounded_window: bool,
        cx: &App,
    ) -> impl IntoElement {
        let dropdown = self.dropdown.clone();
        let query = self.search.read(cx).value();
        let searching = !query.trim().is_empty();
        div()
            .id("playback-settings-panel")
            .debug_selector(|| "playback-settings-panel".into())
            .flex()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            // Keep the panel transparent so the window supplies the rounded background.
            .child(self.render_sidebar(dialog.clone(), searching, rounded_window, cx))
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .child(
                        div()
                            .id("playback-settings-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            .on_scroll_wheel(move |_, _, cx| {
                                dropdown.update(cx, |state, cx| state.close(cx))
                            })
                            .px_8()
                            .child(self.render_settings(dialog, query.trim(), cx)),
                    )
                    .child(
                        div()
                            .id("playback-settings-scrollbar")
                            .debug_selector(|| "playback-settings-scrollbar".into())
                            .absolute()
                            .top_0()
                            .right_0()
                            .bottom_0()
                            .left_0()
                            .child(Scrollbar::vertical(&self.scroll_handle).right_inset(px(4.0))),
                    ),
            )
    }

    fn render_sidebar(
        &self,
        dialog: Entity<Self>,
        searching: bool,
        rounded_window: bool,
        cx: &App,
    ) -> impl IntoElement {
        let theme = theme::get(cx);
        settings_sidebar(rounded_window, cx)
            .child(
                div()
                    .flex()
                    .items_center()
                    .pl_2()
                    .rounded(px(6.0))
                    .overflow_hidden()
                    .border_1()
                    .border_color(theme.input_border)
                    .bg(theme.input_background)
                    .child(
                        svg()
                            .path("icons/search.svg")
                            .size(px(14.0))
                            .flex_shrink_0()
                            .text_color(theme.muted_foreground),
                    )
                    .child(div().flex_1().min_w_0().child(self.search.clone())),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(SettingsCategory::ALL.into_iter().map(|category| {
                        let selected = !searching && self.category == category;
                        let dialog = dialog.clone();
                        settings_category_button(category.title(), selected, cx).on_click(
                            move |_, window, cx| {
                                dialog.update(cx, |dialog, cx| {
                                    // Do not leave focus in an editor that disappears with its page.
                                    window.blur(cx);
                                    dialog.category = category;
                                    dialog.dropdown.update(cx, |state, cx| state.close(cx));
                                    dialog
                                        .search
                                        .update(cx, |search, cx| search.clear_value(cx));
                                    dialog.scroll_handle.set_offset(point(px(0.0), px(0.0)));
                                    cx.notify();
                                });
                            },
                        )
                    })),
            )
    }

    fn render_settings(&self, dialog: Entity<Self>, query: &str, cx: &App) -> impl IntoElement {
        let theme = theme::get(cx);
        let searching = !query.is_empty();
        let items: Vec<_> = self
            .setting_items(dialog, cx)
            .into_iter()
            .filter(|item| {
                if searching {
                    item.matches_query(query)
                } else {
                    item.category == self.category
                }
            })
            .collect();
        let mut content = div().flex().flex_col().min_w_0().child(
            div()
                .mt_2()
                .mb_3()
                .text_base()
                .text_color(theme.foreground)
                .child(if searching {
                    "搜索结果"
                } else {
                    self.category.title()
                }),
        );
        if items.is_empty() {
            return content
                .pt_8()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child("没有找到匹配的设置"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("试试“内存”“预读”或“HTTP”等关键词。"),
                );
        }
        let mut previous_section = None;
        let mut items = items.into_iter().peekable();
        while let Some(item) = items.next() {
            let section = (item.category, item.section);
            if previous_section != Some(section) {
                content = content.child(
                    div()
                        .pb_1p5()
                        .border_b_1()
                        .border_color(theme.title_bar_border)
                        .text_xs()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.muted_foreground)
                        .child(if searching {
                            format!("{} / {}", item.category.title(), item.section)
                        } else {
                            item.section.to_string()
                        }),
                );
                previous_section = Some(section);
            }
            let last_in_section = items
                .peek()
                .is_none_or(|next| (next.category, next.section) != section);
            content = content.child(item.render(last_in_section, cx));
        }
        content
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
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(theme.input_border)
                                .bg(theme.input_background)
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .tooltip(move |_, cx| text_tooltip(tooltip.clone(), cx))
                                .child(div().min_w_0().text_ellipsis().child(path)),
                        )
                }),
        )
    }

    fn setting_items(&self, dialog: Entity<Self>, cx: &App) -> Vec<SettingItem> {
        use SettingsCategory::{Appearance, Disk, General, Memory, Playback, Readahead};

        let toggle = |setting, label, selected| {
            let dialog = dialog.clone();
            toggle_switch(
                label,
                selected,
                move |cx| dialog.update(cx, |dialog, cx| dialog.toggle(setting, cx)),
                cx,
            )
        };
        let cache_dir = SettingItem::new(
            Disk,
            "存储与清理",
            "缓存目录",
            "实际缓存存储目录，由播放器管理，不可修改。",
            "cache_dir path",
            self.render_cache_directories(cx),
        );
        vec![
            SettingItem::new(
                Appearance,
                "主题",
                "颜色主题",
                "切换后立即应用，自动保存你的选择。",
                "color_theme appearance catppuccin latte frappe macchiato mocha",
                selector_row(
                    ("color-theme-dropdown", "颜色主题"),
                    self.dropdown.clone(),
                    ColorTheme::ALL.map(|selection| (selection.id(), selection.name(), selection)),
                    self.color_theme,
                    {
                        let dialog = dialog.clone();
                        move |selection, cx| {
                            dialog.update(cx, |dialog, cx| dialog.select_color_theme(selection, cx))
                        }
                    },
                ),
            ),
            SettingItem::new(
                Playback,
                "首选语言",
                "音轨语言",
                "播放时优先选择此语言；Default 或无匹配时使用默认音轨。",
                "audio preferred language default 音频 语言",
                track_language_selector(
                    ("audio-language-dropdown", "音轨语言"),
                    self.dropdown.clone(),
                    self.track_languages.audio,
                    {
                        let dialog = dialog.clone();
                        move |language, cx| {
                            dialog.update(cx, |dialog, cx| {
                                dialog.select_track_language(language, true, cx)
                            })
                        }
                    },
                ),
            ),
            SettingItem::new(
                Playback,
                "首选语言",
                "字幕语言",
                "播放时优先选择此语言；Default 或无匹配时使用默认字幕，可在详情页手动选择。",
                "subtitle preferred language default 字幕 语言",
                track_language_selector(
                    ("subtitle-language-dropdown", "字幕语言"),
                    self.dropdown.clone(),
                    self.track_languages.subtitle,
                    {
                        let dialog = dialog.clone();
                        move |language, cx| {
                            dialog.update(cx, |dialog, cx| {
                                dialog.select_track_language(language, false, cx)
                            })
                        }
                    },
                ),
            ),
            SettingItem::new(
                Playback,
                "流畅度",
                "解码器追赶丢帧",
                "视频落后时跳过部分解码以追赶音频，可能降低画面连贯性。默认关闭。",
                "decoder_framedrop framedrop 解码 丢帧",
                toggle(
                    ToggleSetting::DecoderFramedrop,
                    "解码器追赶丢帧",
                    self.decoder_framedrop,
                ),
            ),
            SettingItem::new(
                General,
                "缓存模式",
                "普通缓存",
                "自动模式根据媒体来源决定是否启用缓存。",
                "mode auto",
                mode_selector(dialog.clone(), self.mode, self.dropdown.clone()),
            ),
            SettingItem::new(
                General,
                "缓存模式",
                "回看缓存",
                "保留已读取的数据，以便向后跳转和重复播放。",
                "seekable_cache seek",
                seekable_selector(dialog.clone(), self.seekable_cache, self.dropdown.clone()),
            ),
            SettingItem::new(
                General,
                "缓冲与暂停",
                "缓冲不足时暂停",
                "等待缓存补充后再继续播放，减少断续播放。",
                "cache_pause cache-pause",
                toggle(
                    ToggleSetting::CachePause,
                    "缓冲不足时暂停",
                    self.cache_pause,
                ),
            ),
            SettingItem::new(
                General,
                "缓冲与暂停",
                "启动时等待缓存",
                "启用缓冲暂停时，先积累缓存再开始播放。",
                "cache_pause_initial",
                toggle(
                    ToggleSetting::CachePauseInitial,
                    "启动时等待缓存",
                    self.cache_pause_initial,
                ),
            ),
            SettingItem::new(
                General,
                "缓冲与暂停",
                "恢复播放阈值",
                "缓冲暂停后，至少缓存多少秒再恢复播放。",
                "cache_pause_wait",
                NumberControl::new(
                    ("cache-pause-wait", "恢复播放阈值"),
                    self.cache_pause_wait_secs.clone(),
                    "秒",
                    NumberRange::seconds(),
                    self.base_config.cache_pause_wait,
                ),
            ),
            SettingItem::new(
                General,
                "缓冲与暂停",
                "等待预读完成",
                "开始播放前，等待解复用缓存达到预读目标。",
                "demuxer_cache_wait demux",
                toggle(
                    ToggleSetting::DemuxerCacheWait,
                    "等待预读完成",
                    self.demuxer_cache_wait,
                ),
            ),
            SettingItem::new(
                Memory,
                "共享预算",
                "总缓存上限",
                "按 HTTP、前向、回看顺序分配。0 表示各自独立限额。",
                "total_cache_max_bytes",
                NumberControl::new(
                    ("total-cache", "总缓存上限"),
                    self.total_cache_mib.clone(),
                    "MiB",
                    NumberRange::integer(0, 999_999_999_999),
                    bytes_to_mib(self.base_config.total_cache_max_bytes) as f64,
                ),
            ),
            SettingItem::new(
                Memory,
                "HTTP 缓存",
                "网络内存缓存",
                "为已下载的媒体数据保留的内存上限。",
                "http_cache_max_bytes",
                NumberControl::new(
                    ("http-cache", "网络内存缓存"),
                    self.http_cache_mib.clone(),
                    "MiB",
                    NumberRange::integer(0, 999_999_999_999),
                    bytes_to_mib(self.base_config.http_cache_max_bytes) as f64,
                ),
            ),
            SettingItem::new(
                Memory,
                "HTTP 缓存",
                "分页块大小",
                "网络缓存每个数据块的大小。",
                "http_cache_chunk_bytes",
                NumberControl::new(
                    ("http-cache-chunk", "分页块大小"),
                    self.http_cache_chunk_mib.clone(),
                    "MiB",
                    NumberRange::integer(0, 16),
                    bytes_to_mib(self.base_config.http_cache_chunk_bytes) as f64,
                ),
            ),
            SettingItem::new(
                Memory,
                "HTTP 缓存",
                "Range 请求大小",
                "单次 HTTP 范围请求读取的数据量。",
                "http_cache_range_request_bytes",
                NumberControl::new(
                    ("range-request", "Range 请求大小"),
                    self.range_request_mib.clone(),
                    "MiB",
                    NumberRange::integer(0, 128),
                    bytes_to_mib(self.base_config.http_cache_range_request_bytes) as f64,
                ),
            ),
            SettingItem::new(
                Memory,
                "解复用缓存",
                "前向缓存上限",
                "分配前向内存预算；启用磁盘缓存后按此比例扩展媒体窗口。0 表示用尽可用预算。",
                "demuxer_max_bytes demux",
                NumberControl::new(
                    ("demuxer-forward", "前向缓存上限"),
                    self.demuxer_forward_mib.clone(),
                    "MiB",
                    NumberRange::integer(0, 999_999_999_999),
                    bytes_to_mib(self.base_config.demuxer_max_bytes) as f64,
                ),
            ),
            SettingItem::new(
                Memory,
                "解复用缓存",
                "回看缓存上限",
                "分配回看内存预算，并确定磁盘回看空间比例。0 表示关闭。",
                "demuxer_max_back_bytes demux",
                NumberControl::new(
                    ("demuxer-back", "回看缓存上限"),
                    self.demuxer_back_mib.clone(),
                    "MiB",
                    NumberRange::integer(0, 999_999_999_999),
                    bytes_to_mib(self.base_config.demuxer_max_back_bytes) as f64,
                ),
            ),
            SettingItem::new(
                Memory,
                "解复用缓存",
                "保留区间数量",
                "最多保留多少个可供跳转的缓存区间。",
                "demuxer_max_ranges demux range",
                NumberControl::new(
                    ("max-ranges", "保留区间数量"),
                    self.max_ranges.clone(),
                    "个",
                    NumberRange::integer(1, 64),
                    self.base_config.demuxer_max_ranges as f64,
                ),
            ),
            SettingItem::new(
                Memory,
                "解复用缓存",
                "共享空闲前向预算",
                "允许回看缓存使用尚未占用的前向预算。",
                "demuxer_donate_buffer demux",
                toggle(
                    ToggleSetting::DonateBuffer,
                    "共享空闲前向预算",
                    self.demuxer_donate_buffer,
                ),
            ),
            SettingItem::new(
                Disk,
                "存储与清理",
                "启用磁盘缓存",
                "将较远的数据存入磁盘，扩大预读和回看范围。",
                "disk_cache",
                toggle(ToggleSetting::DiskCache, "启用磁盘缓存", self.disk_cache),
            ),
            SettingItem::new(
                Disk,
                "存储与清理",
                "磁盘缓存上限",
                "限制磁盘缓存使用的空间。",
                "disk_cache_max_bytes",
                disk_cache_capacity_control(
                    self.disk_cache_gib.clone(),
                    self.base_config.disk_cache_max_bytes,
                ),
            ),
            cache_dir,
            SettingItem::new(
                Disk,
                "存储与清理",
                "缓存文件清理",
                "选择何时移除磁盘上的缓存文件。",
                "unlink_files",
                unlink_selector(dialog.clone(), self.unlink_files, self.dropdown.clone()),
            ),
            SettingItem::new(
                Readahead,
                "预读时长",
                "网络缓存目标",
                "网络播放时希望提前缓存的时长，仍受容量上限约束。",
                "cache_secs",
                NumberControl::new(
                    ("cache-secs", "网络缓存目标"),
                    self.cache_secs.clone(),
                    "秒",
                    NumberRange::seconds(),
                    self.base_config.cache_secs,
                ),
            ),
            SettingItem::new(
                Readahead,
                "预读时长",
                "解复用预读",
                "提前读取并准备待解码数据的时长。",
                "demuxer_readahead_secs demux",
                NumberControl::new(
                    ("readahead-secs", "解复用预读"),
                    self.readahead_secs.clone(),
                    "秒",
                    NumberRange::seconds(),
                    self.base_config.demuxer_readahead_secs,
                ),
            ),
            SettingItem::new(
                Readahead,
                "预读时长",
                "数据包预读上限",
                "限制数据包提前读取的时长。0 表示不限。",
                "demuxer_packet_max_readahead_secs packet",
                NumberControl::new(
                    ("packet-readahead-secs", "数据包预读上限"),
                    self.packet_readahead_secs.clone(),
                    "秒",
                    NumberRange::seconds(),
                    self.base_config.demuxer_packet_max_readahead_secs,
                ),
            ),
            SettingItem::new(
                Readahead,
                "动态调整",
                "自适应预读",
                "根据下载速度调整网络分段请求大小。",
                "adaptive_readahead",
                toggle(
                    ToggleSetting::AdaptiveReadahead,
                    "自适应预读",
                    self.adaptive_readahead,
                ),
            ),
            SettingItem::new(
                Readahead,
                "动态调整",
                "自动补充阈值",
                "自动决定缓存消耗多少后开始补充，避免频繁读停。",
                "automatic_hysteresis 自动滞回",
                toggle(
                    ToggleSetting::AutomaticHysteresis,
                    "自动补充阈值",
                    self.automatic_hysteresis,
                ),
            ),
            SettingItem::new(
                Readahead,
                "动态调整",
                "补充间隔",
                "缓存消耗多少秒后开始补充。启用自动阈值时，0 表示自动。",
                "demuxer_hysteresis_secs 滞回带",
                NumberControl::new(
                    ("hysteresis-secs", "补充间隔"),
                    self.hysteresis_secs.clone(),
                    "秒",
                    NumberRange::seconds(),
                    self.base_config.demuxer_hysteresis_secs,
                ),
            ),
        ]
    }

    fn select_mode(&mut self, mode: PlaybackCacheMode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.changed(cx);
    }

    fn select_seekable_cache(&mut self, mode: PlaybackSeekableCacheMode, cx: &mut Context<Self>) {
        if self.seekable_cache == mode {
            return;
        }
        self.seekable_cache = mode;
        self.changed(cx);
    }

    fn select_unlink_files(&mut self, policy: CacheUnlinkPolicy, cx: &mut Context<Self>) {
        if self.unlink_files == policy {
            return;
        }
        self.unlink_files = policy;
        self.changed(cx);
    }

    fn toggle(&mut self, setting: ToggleSetting, cx: &mut Context<Self>) {
        match setting {
            ToggleSetting::DecoderFramedrop => self.decoder_framedrop = !self.decoder_framedrop,
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
        self.changed(cx);
    }
}

fn number_input(
    placeholder: &'static str,
    value: u64,
    update_config: fn(&mut PlaybackCacheConfig, u64),
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
            let Ok(value) = input.read(cx).value().trim().parse::<u64>() else {
                return;
            };
            let mut config = this.base_config.clone();
            update_config(&mut config, value);
            let config = config.normalized();
            // Normalize dependent settings, but reject an input that would
            // itself be silently replaced by a different value on save.
            let mut requested = config.clone();
            update_config(&mut requested, value);
            if requested != config {
                return;
            }
            if config != this.base_config {
                this.base_config = config;
                this.changed(cx);
            }
        }
    })
    .detach();
    input
}

fn decimal_input(
    placeholder: &'static str,
    value: f64,
    update_config: fn(&mut PlaybackCacheConfig, f64),
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
            let Some(value) = parse_seconds(input.read(cx).value().as_ref()) else {
                return;
            };
            let mut config = this.base_config.clone();
            update_config(&mut config, value);
            let config = config.normalized();
            if config != this.base_config {
                this.base_config = config;
                this.changed(cx);
            }
        }
    })
    .detach();
    input
}

fn bytes_to_mib(bytes: u64) -> u64 {
    bytes / BYTES_PER_MIB
}

fn parse_seconds(value: &str) -> Option<f64> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
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

fn resolved_cache_directories(
    configured_dir: Option<&Path>,
    environment_dirs: [Option<PathBuf>; 2],
) -> [PathBuf; 2] {
    // Match the HTTP and demux disk caches: configuration, per-cache override,
    // then the application's temporary directory. Only resolve for display.
    environment_dirs.map(|directory| {
        configured_dir
            .map(Path::to_path_buf)
            .or(directory)
            .unwrap_or_else(default_playback_cache_dir)
    })
}

fn matches_search(query: &str, fields: &[&str]) -> bool {
    let haystack = fields.join(" ").to_lowercase();
    query
        .split_whitespace()
        .all(|word| haystack.contains(&word.to_lowercase()))
}

fn mode_selector(
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

fn seekable_selector(
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

fn unlink_selector(
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        BYTES_PER_MIB, bytes_to_mib, format_seconds, parse_seconds, resolved_cache_directories,
    };

    #[test]
    fn byte_units_round_down_for_display() {
        assert_eq!(bytes_to_mib(3 * BYTES_PER_MIB + 1), 3);
    }

    #[test]
    fn incomplete_or_invalid_seconds_are_not_saved() {
        for input in ["", ".", "-", "NaN", "inf", "-1", "1e", "abc"] {
            assert_eq!(parse_seconds(input), None);
        }
        assert_eq!(parse_seconds("0"), Some(0.0));
        assert_eq!(parse_seconds(" 12.5 "), Some(12.5));
    }

    #[test]
    fn seconds_are_displayed_without_unnecessary_zeroes() {
        assert_eq!(format_seconds(2.5), "2.5");
        assert_eq!(format_seconds(2.0), "2");
        assert_eq!(format_seconds(f64::NAN), "0");
    }

    #[test]
    fn cache_directories_resolve_the_default_and_respect_backend_overrides() {
        let default_dir = crate::app_metadata::default_playback_cache_dir();
        assert_eq!(
            resolved_cache_directories(None, [None, None]),
            [default_dir.clone(), default_dir],
        );
        let http = Path::new("/tmp/http-cache");
        let demux = Path::new("/tmp/demux-cache");
        assert_eq!(
            resolved_cache_directories(None, [Some(http.into()), Some(demux.into())]),
            [http.to_path_buf(), demux.to_path_buf()],
        );
        let configured = Path::new("/tmp/configured-cache");
        assert_eq!(
            resolved_cache_directories(Some(configured), [Some(http.into()), Some(demux.into())]),
            [configured.to_path_buf(), configured.to_path_buf()],
        );
    }
}

#[cfg(test)]
#[path = "playback_settings_dialog_tests.rs"]
mod interaction_tests;
