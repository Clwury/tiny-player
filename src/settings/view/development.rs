//! Full settings for development and playback diagnostics.
mod controls;
mod items;
mod layout;
use super::controls::{
    HARDWARE_DECODE_DESCRIPTION, disk_cache_capacity_control, disk_cache_capacity_input,
    hardware_decode_selector, settings_category_button, settings_sidebar, track_language_selector,
};
use super::dialog::SettingsChanged;
use crate::ui::number_input::{NumberControl, NumberRange};
use crate::ui::{
    dropdown::{DropdownState, selector_row},
    toggle::toggle_switch,
};
use controls::{decimal_input, number_input};

use std::path::PathBuf;

#[cfg(test)]
use crate::media::PlaybackLanguagePreferences;
#[cfg(test)]
use tiny_playback::PlaybackCacheConfig;

use gpui::{
    AnyElement, App, AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollHandle, StatefulInteractiveElement, Styled, Subscription, Window,
    div, point, prelude::FluentBuilder, px, relative,
};

use crate::ui::radius;
use crate::{
    app::window_corner_radii,
    media::TrackLanguage,
    theme::{self, ColorTheme},
};
use tiny_playback::{
    CacheUnlinkPolicy, HardwareDecodeMode, PlaybackCacheMode, PlaybackSeekableCacheMode,
};

use crate::ui::{
    editor::{Editor, EditorEvent},
    scrollbar::Scrollbar,
    tooltip::text_tooltip,
};

#[cfg(test)]
use crate::settings::values::{BYTES_PER_MIB, parse_seconds};
use crate::settings::values::{bytes_to_mib, format_seconds, resolved_cache_directories};
use crate::settings::{
    NumericSetting, SettingDescriptor, SettingsCategory, SettingsController, SettingsIntent,
    SettingsMode, SettingsSnapshot, ToggleSetting,
};

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
    pub fn new(snapshot: SettingsSnapshot, cx: &mut Context<Self>) -> Self {
        let config = snapshot.playback.clone().normalized();
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
            controller: SettingsController::from_snapshot(snapshot, SettingsMode::Development),
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
mod interaction_tests;
