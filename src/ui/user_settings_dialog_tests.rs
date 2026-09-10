use gpui::{Modifiers, TestAppContext, VisualTestContext, size};

use super::*;
use crate::{
    player::{CacheUnlinkPolicy, PlaybackCacheMode},
    ui::settings_controls::BYTES_PER_GIB,
};

struct SettingsWindow {
    dialog: Entity<UserSettingsDialogState>,
    saved: PlaybackCacheConfig,
    changes: usize,
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(div().flex_none().child(crate::ui::titlebar::app_titlebar(
                window,
                cx,
                "设置".into(),
            )))
            .child(div().flex_1().min_h_0().child(self.dialog.clone()))
    }
}

fn settings_window(
    cx: &mut TestAppContext,
    config: PlaybackCacheConfig,
) -> (Entity<SettingsWindow>, &mut VisualTestContext) {
    cx.update(|cx| {
        theme::init(cx);
        Editor::bind_keys(cx);
    });
    let (root, cx) = cx.add_window_view(|_, cx| {
        let dialog = cx.new(|cx| UserSettingsDialogState::new(&config, cx));
        cx.subscribe(
            &dialog,
            |root: &mut SettingsWindow, dialog, _: &SettingsChanged, cx| {
                root.saved = dialog.read(cx).playback_config();
                root.changes += 1;
            },
        )
        .detach();
        SettingsWindow {
            dialog,
            saved: config,
            changes: 0,
        }
    });
    cx.simulate_resize(size(px(960.0), px(680.0)));
    cx.run_until_parked();
    (root, cx)
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx.debug_bounds(selector).expect(selector);
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
    for _ in 0..2 {
        cx.update(|window, cx| window.simulate_next_frame(cx));
        cx.run_until_parked();
    }
}

#[gpui::test]
fn user_settings_keep_the_sidebar_and_titlebar_fixed_while_switching_pages(
    cx: &mut TestAppContext,
) {
    let (root, cx) = settings_window(cx, PlaybackCacheConfig::default());
    let dialog = root.read_with(cx, |root, _| root.dialog.clone());
    for (width, height) in [(900.0, 600.0), (960.0, 680.0), (1100.0, 720.0)] {
        cx.simulate_resize(size(px(width), px(height)));
        cx.run_until_parked();
        let panel = cx.debug_bounds("user-settings-panel").unwrap();
        let sidebar = cx.debug_bounds("settings-sidebar").unwrap();
        let close = cx.debug_bounds("window-control-close").unwrap();
        assert_eq!(panel.left(), px(0.0));
        assert_eq!(panel.right(), px(width));
        assert_eq!(panel.top(), px(crate::ui::titlebar::APP_TITLEBAR_HEIGHT_PX));
        assert_eq!(panel.bottom(), px(height));
        assert_eq!(sidebar.left(), panel.left());
        assert_eq!(sidebar.size.width, px(226.0));
        assert_eq!(sidebar.top(), panel.top());
        assert_eq!(sidebar.bottom(), panel.bottom());
        let pages: [(&str, &[&str]); 4] = [
            (
                "settings-category-外观",
                &["user-setting-theme", "color-theme-dropdown"],
            ),
            (
                "settings-category-播放",
                &[
                    "user-setting-audio",
                    "user-setting-subtitle",
                    "audio-language-dropdown",
                    "subtitle-language-dropdown",
                ],
            ),
            (
                "settings-category-内存缓存",
                &["user-setting-memory-budget", "memory-budget-dropdown"],
            ),
            (
                "settings-category-磁盘缓存",
                &[
                    "user-setting-disk",
                    "user-setting-disk-limit",
                    "settings-toggle-启用磁盘缓存",
                    "disk-cache",
                ],
            ),
        ];
        for (category, controls) in pages {
            click(cx, category);
            let scroll = dialog.read_with(cx, |dialog, _| dialog.scroll_handle.clone());
            assert_eq!(scroll.bounds().left(), sidebar.right());
            assert_eq!(scroll.bounds().right(), panel.right());
            assert_eq!(scroll.bounds().top(), panel.top());
            assert_eq!(scroll.bounds().bottom(), panel.bottom());
            for selector in controls {
                let bounds = cx.debug_bounds(selector).expect(selector);
                assert!(bounds.left() >= scroll.bounds().left(), "{selector}");
                assert!(bounds.right() <= scroll.bounds().right(), "{selector}");
                assert!(bounds.top() >= scroll.bounds().top(), "{selector}");
                assert!(bounds.bottom() <= scroll.bounds().bottom(), "{selector}");
            }
            assert_eq!(cx.debug_bounds("settings-sidebar").unwrap(), sidebar);
            assert_eq!(cx.debug_bounds("window-control-close").unwrap(), close);
        }
        for selector in [
            "playback-settings-panel",
            "settings-category-预读策略",
            "settings-toggle-解码器追赶丢帧",
            "cache-secs-input",
        ] {
            assert!(cx.debug_bounds(selector).is_none());
        }
    }
    assert_eq!(root.read_with(cx, |root, _| root.changes), 0);
}

#[gpui::test]
fn theme_and_language_edits_preserve_custom_playback_settings_at_full_precision(
    cx: &mut TestAppContext,
) {
    let config = PlaybackCacheConfig {
        mode: PlaybackCacheMode::Disabled,
        total_cache_max_bytes: 192 * 1024 * 1024,
        cache_secs: 12.34567,
        http_cache_max_bytes: 33 * 1024 * 1024 + 123,
        disk_cache: true,
        disk_cache_max_bytes: 5 * BYTES_PER_GIB + 123,
        unlink_files: CacheUnlinkPolicy::Never,
        decoder_framedrop: true,
        ..PlaybackCacheConfig::default()
    };
    let (root, cx) = settings_window(cx, config.clone());
    click(cx, "settings-category-内存缓存");
    click(cx, "memory-budget-dropdown");
    click(cx, "memory-budget-custom");
    assert_eq!(root.read_with(cx, |root, _| root.changes), 0);
    click(cx, "settings-category-外观");
    click(cx, "color-theme-dropdown");
    click(cx, "theme-latte");
    assert_eq!(
        cx.update(|_, cx| theme::get(cx).selection),
        ColorTheme::Latte
    );
    click(cx, "settings-category-播放");
    click(cx, "audio-language-dropdown");
    click(cx, "language-japanese");
    click(cx, "subtitle-language-dropdown");
    click(cx, "language-chinese-simplified");
    assert_eq!(
        cx.update(|_, cx| PlaybackLanguagePreferences::get(cx)),
        PlaybackLanguagePreferences {
            audio: TrackLanguage::Japanese,
            subtitle: TrackLanguage::ChineseSimplified,
        }
    );
    assert_eq!(root.read_with(cx, |root, _| root.saved.clone()), config);
    assert_eq!(root.read_with(cx, |root, _| root.changes), 3);
}

#[gpui::test]
fn memory_capacity_options_apply_by_mouse_and_keyboard_without_enabling_decoder_dropping(
    cx: &mut TestAppContext,
) {
    let (root, cx) = settings_window(cx, PlaybackCacheConfig::default());
    click(cx, "settings-category-内存缓存");
    click(cx, "memory-budget-dropdown");
    for budget in MemoryBudget::ALL {
        let bounds = cx
            .debug_bounds(budget.id())
            .unwrap_or_else(|| panic!("missing capacity option: {}", budget.label()));
        assert!(bounds.top() >= px(0.0));
        assert!(bounds.bottom() <= px(680.0));
    }
    click(cx, "memory-budget-256-mib");
    assert_eq!(root.read_with(cx, |root, _| root.changes), 0);
    click(cx, "memory-budget-dropdown");
    cx.simulate_keystrokes("end enter");
    cx.run_until_parked();
    assert_eq!(
        root.read_with(cx, |root, _| root.saved.total_cache_max_bytes),
        2 * BYTES_PER_GIB
    );
    click(cx, "settings-category-磁盘缓存");
    click(cx, "settings-toggle-启用磁盘缓存");
    assert!(root.read_with(cx, |root, _| root.saved.disk_cache));
    click(cx, "settings-category-内存缓存");
    click(cx, "memory-budget-dropdown");
    click(cx, "memory-budget-128-mib");
    let saved = root.read_with(cx, |root, _| root.saved.clone());
    assert_eq!(saved.total_cache_max_bytes, 128 * 1024 * 1024);
    assert!(saved.disk_cache);
    assert!(!saved.decoder_framedrop);
    assert_eq!(root.read_with(cx, |root, _| root.changes), 3);
}

#[gpui::test]
fn disk_cache_switch_preserves_custom_capacity_and_capacity_can_be_edited_while_disabled(
    cx: &mut TestAppContext,
) {
    let config = PlaybackCacheConfig {
        disk_cache_max_bytes: 5 * BYTES_PER_GIB + 123,
        unlink_files: CacheUnlinkPolicy::Never,
        ..PlaybackCacheConfig::default()
    };
    let (root, cx) = settings_window(cx, config.clone());
    let input = root.read_with(cx, |root, cx| root.dialog.read(cx).disk_cache_gib.clone());
    click(cx, "settings-category-磁盘缓存");
    assert_eq!(input.read_with(cx, |input, _| input.value()), "5");
    click(cx, "settings-toggle-启用磁盘缓存");
    assert_eq!(
        root.read_with(cx, |root, _| root.saved.disk_cache_max_bytes),
        config.disk_cache_max_bytes
    );
    click(cx, "disk-cache-input");
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("12");
    cx.run_until_parked();
    click(cx, "settings-toggle-启用磁盘缓存");
    assert!(!root.read_with(cx, |root, _| root.saved.disk_cache));
    assert_eq!(
        root.read_with(cx, |root, _| root.saved.disk_cache_max_bytes),
        12 * BYTES_PER_GIB
    );
    click(cx, "disk-cache-increment");
    assert_eq!(input.read_with(cx, |input, _| input.value()), "13");
    click(cx, "settings-category-外观");
    click(cx, "settings-category-磁盘缓存");
    let saved = root.read_with(cx, |root, _| root.saved.clone());
    assert_eq!(
        saved,
        PlaybackCacheConfig {
            disk_cache_max_bytes: 13 * BYTES_PER_GIB,
            ..config
        }
    );
    assert_eq!(input.read_with(cx, |input, _| input.value()), "13");

    let changes = root.read_with(cx, |root, _| root.changes);
    for invalid in ["", "0", "17179869184"] {
        input.update(cx, |input, cx| input.set_value(invalid, cx));
        cx.run_until_parked();
        assert_eq!(root.read_with(cx, |root, _| root.changes), changes);
        assert_eq!(root.read_with(cx, |root, _| root.saved.clone()), saved);
    }
    for value in [1, u64::MAX / BYTES_PER_GIB] {
        input.update(cx, |input, cx| input.set_value(value.to_string(), cx));
        cx.run_until_parked();
        assert_eq!(
            root.read_with(cx, |root, _| root.saved.disk_cache_max_bytes),
            value * BYTES_PER_GIB
        );
    }
    click(cx, "disk-cache-increment");
    assert_eq!(
        root.read_with(cx, |root, _| root.saved.disk_cache_max_bytes),
        (u64::MAX / BYTES_PER_GIB) * BYTES_PER_GIB
    );
}

#[gpui::test]
fn switching_sidebar_pages_dismisses_menus_and_blurs_hidden_editors(cx: &mut TestAppContext) {
    let (root, cx) = settings_window(cx, PlaybackCacheConfig::default());
    click(cx, "color-theme-dropdown");
    click(cx, "settings-category-磁盘缓存");
    assert!(cx.debug_bounds("color-theme-dropdown-menu").is_none());
    let input = root.read_with(cx, |root, cx| root.dialog.read(cx).disk_cache_gib.clone());
    click(cx, "disk-cache-input");
    assert!(cx.update(|window, cx| input.read(cx).focus_handle(cx).is_focused(window)));
    click(cx, "settings-category-播放");
    assert!(!cx.update(|window, cx| input.read(cx).focus_handle(cx).is_focused(window)));
    assert!(cx.debug_bounds("disk-cache-input").is_none());
    assert!(cx.debug_bounds("audio-language-dropdown").is_some());
    assert_eq!(root.read_with(cx, |root, _| root.changes), 0);
}

#[gpui::test]
fn language_menus_offer_every_dev_language_at_the_minimum_window_size(cx: &mut TestAppContext) {
    let (_, cx) = settings_window(cx, PlaybackCacheConfig::default());
    cx.simulate_resize(size(px(900.0), px(600.0)));
    cx.run_until_parked();
    click(cx, "settings-category-播放");
    for selector in ["audio-language-dropdown", "subtitle-language-dropdown"] {
        click(cx, selector);
        for language in TrackLanguage::ALL {
            let bounds = cx.debug_bounds(language.id()).unwrap();
            assert!(bounds.top() >= px(0.0));
            assert!(bounds.bottom() <= px(600.0));
        }
        cx.simulate_keystrokes("end enter");
        cx.run_until_parked();
        click(cx, selector);
        click(cx, "language-default");
    }
    assert_eq!(
        cx.update(|_, cx| PlaybackLanguagePreferences::get(cx)),
        PlaybackLanguagePreferences::default()
    );
}
