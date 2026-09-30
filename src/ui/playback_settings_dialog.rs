//! Full settings for development and playback diagnostics.

use std::path::PathBuf;

use gpui::{
    AnyElement, App, AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollHandle, StatefulInteractiveElement, Styled, Subscription, Window,
    div, point, prelude::FluentBuilder, px, relative,
};

use crate::ui::radius;
use crate::{
    app::window_corner_radii,
    player::{
        CacheUnlinkPolicy, HardwareDecodeMode, PlaybackCacheConfig, PlaybackCacheMode,
        PlaybackLanguagePreferences, PlaybackSeekableCacheMode, TrackLanguage,
    },
    theme::{self, ColorTheme},
};

use super::{
    editor::{Editor, EditorEvent},
    scrollbar::Scrollbar,
    settings_controls::{
        DropdownState, HARDWARE_DECODE_DESCRIPTION, NumberControl, NumberRange,
        disk_cache_capacity_control, disk_cache_capacity_input, hardware_decode_selector,
        selector_row, settings_category_button, settings_sidebar, toggle_switch,
        track_language_selector,
    },
    settings_dialog::SettingsChanged,
    tooltip::text_tooltip,
};

#[cfg(test)]
use crate::settings::values::{BYTES_PER_MIB, parse_seconds};
use crate::settings::values::{bytes_to_mib, format_seconds, resolved_cache_directories};
use crate::settings::{
    NumericSetting, SettingDescriptor, SettingsCategory, SettingsController, SettingsIntent,
    SettingsMode, SettingsSnapshot, ToggleSetting,
};

struct SettingItem {
    descriptor: SettingDescriptor,
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
            descriptor: SettingDescriptor {
                category,
                section,
                title,
                description,
                keywords,
            },
            control: control.into_any_element(),
        }
    }

    fn render(self, last_in_section: bool, cx: &App) -> impl IntoElement {
        let theme = theme::get(cx);
        div()
            .id(self.descriptor.title)
            .debug_selector(move || self.descriptor.title.into())
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
                            .child(self.descriptor.title),
                    )
                    .child(
                        div()
                            .text_xs()
                            .line_height(relative(1.5))
                            .text_color(theme.muted_foreground)
                            .child(self.descriptor.description),
                    ),
            )
            .child(div().flex_shrink_0().child(self.control))
    }
}

pub struct PlaybackSettingsDialogState {
    controller: SettingsController,
    search: Entity<Editor>,
    dropdown: Entity<DropdownState>,
    scroll_handle: ScrollHandle,
    _search_subscription: Subscription,
    cache_directories: [PathBuf; 2],
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
        let corners = window_corner_radii(window, cx);
        self.render_content(cx.entity(), corners.bottom_left, cx)
    }
}

impl PlaybackSettingsDialogState {
    pub fn new(config: &PlaybackCacheConfig, cx: &mut Context<Self>) -> Self {
        let config = config.clone().normalized();
        let search = cx.new(|cx| Editor::new("搜索设置…", cx).search());
        let scroll_handle = ScrollHandle::new();
        let search_subscription = cx.subscribe(&search, |this, search, event, cx| {
            if matches!(event, EditorEvent::Changed) {
                this.dispatch(
                    SettingsIntent::Search(search.read(cx).value().to_string()),
                    cx,
                );
                this.dropdown.update(cx, |state, cx| state.close(cx));
                this.scroll_handle.set_offset(point(px(0.0), px(0.0)));
                cx.notify();
            }
        });
        cx.on_release(|dialog, _| {
            dialog.controller.dispatch(SettingsIntent::Close);
        })
        .detach();
        Self {
            controller: SettingsController::new(
                &config,
                SettingsMode::Development,
                theme::get(cx).selection,
                PlaybackLanguagePreferences::get(cx),
            ),
            search,
            dropdown: cx.new(DropdownState::new),
            scroll_handle,
            _search_subscription: search_subscription,
            cache_directories: resolved_cache_directories(
                config.cache_dir.as_deref(),
                ["TINY_HTTP_CACHE_DIR", "TINY_DEMUX_PACKET_CACHE_DIR"]
                    .map(|key| std::env::var(key).ok().map(PathBuf::from)),
            ),
            total_cache_mib: number_input(
                "总缓存上限（MiB，0=独立上限）",
                bytes_to_mib(config.total_cache_max_bytes),
                NumericSetting::TotalCacheMib,
                cx,
            ),
            http_cache_mib: number_input(
                "HTTP 内存缓存（MiB）",
                bytes_to_mib(config.http_cache_max_bytes),
                NumericSetting::HttpCacheMib,
                cx,
            ),
            http_cache_chunk_mib: number_input(
                "HTTP 分页块（MiB）",
                bytes_to_mib(config.http_cache_chunk_bytes),
                NumericSetting::HttpCacheChunkMib,
                cx,
            ),
            demuxer_forward_mib: number_input(
                "Demux 前向缓存（MiB，0=不限）",
                bytes_to_mib(config.demuxer_max_bytes),
                NumericSetting::DemuxerForwardMib,
                cx,
            ),
            demuxer_back_mib: number_input(
                "Demux 回看缓存（MiB，0=关闭）",
                bytes_to_mib(config.demuxer_max_back_bytes),
                NumericSetting::DemuxerBackMib,
                cx,
            ),
            range_request_mib: number_input(
                "HTTP Range 请求（MiB）",
                bytes_to_mib(config.http_cache_range_request_bytes),
                NumericSetting::RangeRequestMib,
                cx,
            ),
            cache_secs: decimal_input(
                "网络缓存目标（秒）",
                config.cache_secs,
                NumericSetting::CacheSecs,
                cx,
            ),
            readahead_secs: decimal_input(
                "Demux 预读（秒）",
                config.demuxer_readahead_secs,
                NumericSetting::ReadaheadSecs,
                cx,
            ),
            packet_readahead_secs: decimal_input(
                "Packet 预读上限（秒，0=不限）",
                config.demuxer_packet_max_readahead_secs,
                NumericSetting::PacketReadaheadSecs,
                cx,
            ),
            hysteresis_secs: decimal_input(
                "滞回带（秒，自动模式下0=自动）",
                config.demuxer_hysteresis_secs,
                NumericSetting::HysteresisSecs,
                cx,
            ),
            cache_pause_wait_secs: decimal_input(
                "Cache-pause 恢复阈值（秒）",
                config.cache_pause_wait,
                NumericSetting::CachePauseWaitSecs,
                cx,
            ),
            max_ranges: number_input(
                "最多保留 Demux range",
                config.demuxer_max_ranges as u64,
                NumericSetting::MaxRanges,
                cx,
            ),
            disk_cache_gib: disk_cache_capacity_input(
                config.disk_cache_max_bytes,
                |this, input, cx| {
                    this.dispatch(
                        SettingsIntent::EditNumber {
                            field: NumericSetting::DiskCacheGib,
                            input,
                        },
                        cx,
                    )
                },
                cx,
            ),
        }
    }

    #[cfg(test)]
    pub fn color_theme(&self) -> ColorTheme {
        self.controller.view_model().color_theme
    }
    #[cfg(test)]
    pub fn track_languages(&self) -> PlaybackLanguagePreferences {
        self.controller.view_model().track_languages
    }
    #[cfg(test)]
    pub fn playback_config(&self) -> PlaybackCacheConfig {
        self.controller.view_model().config.clone()
    }
    pub(crate) fn snapshot(&self) -> SettingsSnapshot {
        self.controller.snapshot()
    }

    fn dispatch(&mut self, intent: SettingsIntent, cx: &mut Context<Self>) {
        let change = self.controller.dispatch(intent);
        if let Some(selection) = change.theme {
            theme::set(selection, cx);
            self.controller
                .finish_theme_selection(theme::get(cx).selection);
        }
        if let Some(languages) = change.languages {
            languages.apply(cx);
        }
        if change.persist {
            cx.emit(SettingsChanged);
        }
        if change.view_changed {
            cx.notify();
        }
    }
    fn select_track_language(
        &mut self,
        language: TrackLanguage,
        audio: bool,
        cx: &mut Context<Self>,
    ) {
        self.dispatch(SettingsIntent::Language { language, audio }, cx);
    }
    fn select_color_theme(&mut self, selection: ColorTheme, cx: &mut Context<Self>) {
        self.dispatch(SettingsIntent::Theme(selection), cx);
    }
    fn select_hardware_decode(&mut self, mode: HardwareDecodeMode, cx: &mut Context<Self>) {
        self.dispatch(SettingsIntent::HardwareDecode(mode), cx);
    }

    fn render_content(
        &self,
        dialog: Entity<Self>,
        bottom_left_radius: gpui::Pixels,
        cx: &App,
    ) -> impl IntoElement {
        let dropdown = self.dropdown.clone();
        let query = self.controller.view_model().query;
        let searching = !query.trim().is_empty();
        div()
            .id("playback-settings-panel")
            .debug_selector(|| "playback-settings-panel".into())
            .flex()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            // Keep the panel transparent so the window supplies the rounded background.
            .child(self.render_sidebar(dialog.clone(), searching, bottom_left_radius, cx))
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
        bottom_left_radius: gpui::Pixels,
        cx: &App,
    ) -> impl IntoElement {
        settings_sidebar(bottom_left_radius, cx)
            .child(self.search.clone())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(SettingsCategory::ALL.into_iter().map(|category| {
                        let selected =
                            !searching && self.controller.view_model().category == category;
                        let dialog = dialog.clone();
                        settings_category_button(category.title(), selected, cx).on_click(
                            move |_, window, cx| {
                                dialog.update(cx, |dialog, cx| {
                                    // Do not leave focus in an editor that disappears with its page.
                                    window.blur(cx);
                                    dialog.dispatch(SettingsIntent::Category(category), cx);
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
            .filter(|item| self.controller.view_model().includes(&item.descriptor))
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
                    self.controller.view_model().category.title()
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
            let section = (item.descriptor.category, item.descriptor.section);
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
                            format!(
                                "{} / {}",
                                item.descriptor.category.title(),
                                item.descriptor.section
                            )
                        } else {
                            item.descriptor.section.to_string()
                        }),
                );
                previous_section = Some(section);
            }
            let last_in_section = items
                .peek()
                .is_none_or(|next| (next.descriptor.category, next.descriptor.section) != section);
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
                    self.controller.view_model().color_theme,
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
                    self.controller.view_model().track_languages.audio,
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
                    self.controller.view_model().track_languages.subtitle,
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
                "视频解码",
                "硬件解码",
                HARDWARE_DECODE_DESCRIPTION,
                "hardware_decode hwdec vulkan 硬解 软解",
                hardware_decode_selector(
                    self.dropdown.clone(),
                    self.controller.view_model().config.hardware_decode,
                    {
                        let dialog = dialog.clone();
                        move |mode, cx| {
                            dialog.update(cx, |dialog, cx| dialog.select_hardware_decode(mode, cx))
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
                    self.controller.view_model().config.decoder_framedrop,
                ),
            ),
            SettingItem::new(
                General,
                "缓存模式",
                "普通缓存",
                "自动模式根据媒体来源决定是否启用缓存。",
                "mode auto",
                mode_selector(
                    dialog.clone(),
                    self.controller.view_model().config.mode,
                    self.dropdown.clone(),
                ),
            ),
            SettingItem::new(
                General,
                "缓存模式",
                "回看缓存",
                "保留已读取的数据，以便向后跳转和重复播放。",
                "seekable_cache seek",
                seekable_selector(
                    dialog.clone(),
                    self.controller.view_model().config.seekable_cache,
                    self.dropdown.clone(),
                ),
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
                    self.controller.view_model().config.cache_pause,
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
                    self.controller.view_model().config.cache_pause_initial,
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
                    self.controller.view_model().config.cache_pause_wait,
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
                    self.controller.view_model().config.demuxer_cache_wait,
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
                    bytes_to_mib(self.controller.view_model().config.total_cache_max_bytes) as f64,
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
                    bytes_to_mib(self.controller.view_model().config.http_cache_max_bytes) as f64,
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
                    bytes_to_mib(self.controller.view_model().config.http_cache_chunk_bytes) as f64,
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
                    bytes_to_mib(
                        self.controller
                            .view_model()
                            .config
                            .http_cache_range_request_bytes,
                    ) as f64,
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
                    bytes_to_mib(self.controller.view_model().config.demuxer_max_bytes) as f64,
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
                    bytes_to_mib(self.controller.view_model().config.demuxer_max_back_bytes) as f64,
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
                    self.controller.view_model().config.demuxer_max_ranges as f64,
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
                    self.controller.view_model().config.demuxer_donate_buffer,
                ),
            ),
            SettingItem::new(
                Disk,
                "存储与清理",
                "启用磁盘缓存",
                "将较远的数据存入磁盘，扩大预读和回看范围。",
                "disk_cache",
                toggle(
                    ToggleSetting::DiskCache,
                    "启用磁盘缓存",
                    self.controller.view_model().config.disk_cache,
                ),
            ),
            SettingItem::new(
                Disk,
                "存储与清理",
                "磁盘缓存上限",
                "限制磁盘缓存使用的空间。",
                "disk_cache_max_bytes",
                disk_cache_capacity_control(
                    self.disk_cache_gib.clone(),
                    self.controller.view_model().config.disk_cache_max_bytes,
                ),
            ),
            cache_dir,
            SettingItem::new(
                Disk,
                "存储与清理",
                "缓存文件清理",
                "选择何时移除磁盘上的缓存文件。",
                "unlink_files",
                unlink_selector(
                    dialog.clone(),
                    self.controller.view_model().config.unlink_files,
                    self.dropdown.clone(),
                ),
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
                    self.controller.view_model().config.cache_secs,
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
                    self.controller.view_model().config.demuxer_readahead_secs,
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
                    self.controller
                        .view_model()
                        .config
                        .demuxer_packet_max_readahead_secs,
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
                    self.controller.view_model().config.adaptive_readahead,
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
                    self.controller.view_model().config.automatic_hysteresis,
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
                    self.controller.view_model().config.demuxer_hysteresis_secs,
                ),
            ),
        ]
    }

    fn select_mode(&mut self, mode: PlaybackCacheMode, cx: &mut Context<Self>) {
        self.dispatch(SettingsIntent::Mode(mode), cx);
    }
    fn select_seekable_cache(&mut self, mode: PlaybackSeekableCacheMode, cx: &mut Context<Self>) {
        self.dispatch(SettingsIntent::SeekableCache(mode), cx);
    }
    fn select_unlink_files(&mut self, policy: CacheUnlinkPolicy, cx: &mut Context<Self>) {
        self.dispatch(SettingsIntent::UnlinkFiles(policy), cx);
    }
    fn toggle(&mut self, setting: ToggleSetting, cx: &mut Context<Self>) {
        self.dispatch(SettingsIntent::Toggle(setting), cx);
    }
}

fn number_input(
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

fn decimal_input(
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
