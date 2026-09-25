use gpui::{
    AppContext as _, Context, Entity, IntoElement, Modifiers, ParentElement, Render, Styled,
    TestAppContext, VisualTestContext, Window, div, px, size,
};

use super::{BYTES_PER_MIB, PlaybackSettingsDialogState, SettingsChanged, matches_search};
use crate::{
    player::{CacheUnlinkPolicy, PlaybackCacheConfig, PlaybackCacheMode},
    theme::{self, ColorTheme},
    ui::editor::Editor,
};

struct SettingsWindow {
    dialog: Entity<PlaybackSettingsDialogState>,
    saved: Option<PlaybackCacheConfig>,
    saved_theme: Option<ColorTheme>,
    change_count: usize,
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
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

fn settings_window(cx: &mut TestAppContext) -> (Entity<SettingsWindow>, &mut VisualTestContext) {
    settings_window_with_config(cx, PlaybackCacheConfig::default())
}

fn settings_window_with_config(
    cx: &mut TestAppContext,
    config: PlaybackCacheConfig,
) -> (Entity<SettingsWindow>, &mut VisualTestContext) {
    cx.update(|cx| {
        theme::init(cx);
        Editor::bind_keys(cx);
    });
    cx.add_window_view(|_, cx| {
        let dialog = cx.new(|cx| PlaybackSettingsDialogState::new(&config, cx));
        cx.subscribe(
            &dialog,
            |root: &mut SettingsWindow, dialog, _: &SettingsChanged, cx| {
                root.saved = Some(dialog.read(cx).playback_config());
                root.saved_theme = Some(dialog.read(cx).color_theme());
                root.change_count += 1;
                cx.notify();
            },
        )
        .detach();
        SettingsWindow {
            dialog,
            saved: Some(config),
            saved_theme: Some(ColorTheme::Mocha),
            change_count: 0,
        }
    })
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx.debug_bounds(selector).expect(selector);
    if selector == "window-control-close" {
        if cfg!(target_os = "windows") {
            // The test platform doesn't dispatch Win32 non-client messages.
            // Exercise the same window removal that GPUI's backend performs.
            cx.update(|window, _| window.remove_window());
        } else {
            cx.simulate_click(bounds.center(), Modifiers::default());
        }
        cx.run_until_parked();
        return;
    }
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
    settle_menu_frames(cx);
}

fn settle_menu_frames(cx: &mut VisualTestContext) {
    // Popover focus is deferred until the menu is in the dispatch tree, as in Zed.
    for _ in 0..2 {
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        cx.run_until_parked();
    }
}

#[gpui::test]
fn latte_selected_categories_dropdowns_and_switches_have_hover_feedback(cx: &mut TestAppContext) {
    let (root, cx) = settings_window(cx);
    cx.update(|_, cx| theme::set(ColorTheme::Latte, cx));
    cx.simulate_resize(size(px(1100.0), px(720.0)));
    cx.run_until_parked();
    for selector in ["settings-category-外观", "color-theme-dropdown"] {
        let bounds = cx.debug_bounds(selector).unwrap();
        cx.simulate_mouse_move(bounds.center(), None, Modifiers::default());
        cx.run_until_parked();
        assert!(cx.update(|window, cx| {
            let theme = theme::get(cx);
            let expected = if selector == "settings-category-外观" {
                theme.element_selected_hover
            } else {
                theme.secondary_hover
            };
            window
                .painted_quads()
                .iter()
                .any(|quad| quad.background == expected.into())
        }));
    }
    click(cx, "settings-category-磁盘缓存");
    let selector = "settings-toggle-启用磁盘缓存";
    for _ in 0..2 {
        cx.simulate_mouse_move(
            gpui::point(px(400.0), px(400.0)),
            None,
            Modifiers::default(),
        );
        cx.run_until_parked();
        let bounds = cx.debug_bounds(selector).unwrap();
        cx.simulate_mouse_move(bounds.center(), None, Modifiers::default());
        cx.run_until_parked();
        assert!(cx.update(|window, cx| {
            let theme = theme::get(cx);
            let enabled = root.read(cx).dialog.read(cx).playback_config().disk_cache;
            let expected = if enabled {
                theme.accent_hover
            } else {
                theme.secondary_hover
            };
            let quads = window.painted_quads();
            quads.iter().any(|fill| {
                fill.background == expected.into()
                    && quads.iter().any(|border| {
                        border.bounds == fill.bounds && border.border_color == theme.accent
                    })
            })
        }));
        click(cx, selector);
    }
}

#[test]
fn search_matches_chinese_and_multiple_case_insensitive_keywords() {
    let fields = [
        "内存缓存",
        "Range 请求大小",
        "http_cache_range_request_bytes",
    ];
    assert!(matches_search("  HTTP range  ", &fields));
    assert!(matches_search("内存 请求", &fields));
    assert!(matches_search(" \t ", &fields));
    assert!(!matches_search("HTTP 磁盘", &fields));
    assert!(!matches_search("not-a-setting", &fields));
}

#[gpui::test]
fn decoder_framedrop_toggle_is_off_by_default_and_saves_each_change(cx: &mut TestAppContext) {
    let (root, cx) = settings_window(cx);
    cx.simulate_resize(size(px(960.0), px(760.0)));
    cx.run_until_parked();
    click(cx, "settings-category-播放");
    assert!(!root.read_with(cx, |root, _| root.saved.as_ref().unwrap().decoder_framedrop));
    click(cx, "settings-toggle-解码器追赶丢帧");
    assert!(root.read_with(cx, |root, _| root.saved.as_ref().unwrap().decoder_framedrop));
    click(cx, "settings-toggle-解码器追赶丢帧");
    assert!(!root.read_with(cx, |root, _| root.saved.as_ref().unwrap().decoder_framedrop));
    assert_eq!(root.read_with(cx, |root, _| root.change_count), 2);
}

#[gpui::test]
fn language_dropdowns_default_to_default_and_offer_all_reference_languages(
    cx: &mut TestAppContext,
) {
    use crate::player::{PlaybackLanguagePreferences, TrackLanguage};

    let (root, cx) = settings_window(cx);
    cx.simulate_resize(size(px(900.0), px(600.0)));
    cx.run_until_parked();
    click(cx, "settings-category-播放");
    let dialog = root.read_with(cx, |root, _| root.dialog.clone());
    assert_eq!(
        dialog.read_with(cx, |dialog, _| dialog.track_languages()),
        PlaybackLanguagePreferences::default()
    );
    for selector in ["audio-language-dropdown", "subtitle-language-dropdown"] {
        click(cx, selector);
        for language in TrackLanguage::ALL {
            let bounds = cx
                .debug_bounds(language.id())
                .unwrap_or_else(|| panic!("missing language option: {}", language.label()));
            assert!(bounds.origin.y >= px(0.0));
            assert!(bounds.bottom() <= px(600.0));
        }
        cx.simulate_keystrokes("end enter");
        cx.run_until_parked();
        assert!(cx.debug_bounds("language-russian").is_none());
        click(cx, selector);
        click(cx, "language-default");
    }
    assert_eq!(
        cx.update(|_, cx| PlaybackLanguagePreferences::get(cx)),
        PlaybackLanguagePreferences::default()
    );
}

#[gpui::test]
fn theme_selection_applies_and_autosaves_without_reverting_on_close(cx: &mut TestAppContext) {
    let (root, cx) = settings_window(cx);
    cx.simulate_resize(size(px(1100.0), px(720.0)));
    cx.run_until_parked();
    click(cx, "color-theme-dropdown");
    // Exercise a fourth option, including wrapping and Home/End navigation.
    assert!(cx.debug_bounds("theme-mocha").is_some());
    click(cx, "theme-latte");
    assert_eq!(
        cx.update(|_, cx| theme::get(cx).selection),
        ColorTheme::Latte
    );
    assert_eq!(
        root.read_with(cx, |root, _| root.saved_theme),
        Some(ColorTheme::Latte)
    );
    assert!(cx.debug_bounds("color-theme-dropdown-menu").is_none());

    click(cx, "color-theme-dropdown");
    cx.simulate_keystrokes("end up enter");
    assert_eq!(
        cx.update(|_, cx| theme::get(cx).selection),
        ColorTheme::Macchiato
    );
    click(cx, "color-theme-dropdown");
    cx.simulate_keystrokes("end down enter");
    assert_eq!(
        cx.update(|_, cx| theme::get(cx).selection),
        ColorTheme::Latte
    );
    click(cx, "color-theme-dropdown");
    cx.simulate_keystrokes("home up enter");
    assert_eq!(
        cx.update(|_, cx| theme::get(cx).selection),
        ColorTheme::Mocha
    );
    click(cx, "color-theme-dropdown");
    click(cx, "theme-frappe");
    assert_eq!(
        cx.update(|_, cx| theme::get(cx).selection),
        ColorTheme::Frappe
    );
    click(cx, "window-control-close");
    assert_eq!(
        cx.cx.update(|cx| theme::get(cx).selection),
        ColorTheme::Frappe
    );
    assert_eq!(
        root.read_with(cx, |root, _| root.saved_theme),
        Some(ColorTheme::Frappe)
    );
    assert!(cx.windows().is_empty());
}

#[gpui::test]
fn theme_can_be_found_by_search_and_saved_without_changing_playback_settings(
    cx: &mut TestAppContext,
) {
    let (root, cx) = settings_window(cx);
    cx.simulate_resize(size(px(1100.0), px(720.0)));
    cx.run_until_parked();
    let search = root.read_with(cx, |root, cx| root.dialog.read(cx).search.clone());
    cx.update(|window, cx| search.read(cx).focus_handle(cx).focus(window, cx));
    cx.simulate_input("color_theme");
    assert!(cx.debug_bounds("颜色主题").is_some());
    click(cx, "color-theme-dropdown");
    click(cx, "theme-latte");
    click(cx, "settings-category-内存缓存");
    assert_eq!(
        cx.update(|_, cx| theme::get(cx).selection),
        ColorTheme::Latte
    );
    assert_eq!(
        root.read_with(cx, |root, _| root.saved_theme),
        Some(ColorTheme::Latte)
    );
    assert_eq!(
        root.read_with(cx, |root, _| root.saved.clone()),
        Some(PlaybackCacheConfig::default())
    );
    let reopened = cx.update(|_, cx| {
        cx.new(|cx| PlaybackSettingsDialogState::new(&PlaybackCacheConfig::default(), cx))
    });
    assert_eq!(
        reopened.read_with(cx, |dialog, _| dialog.color_theme()),
        ColorTheme::Latte
    );
}

#[gpui::test]
fn search_and_category_changes_preserve_automatically_saved_edits(cx: &mut TestAppContext) {
    let (root, cx) = settings_window(cx);
    cx.simulate_resize(size(px(1100.0), px(720.0)));
    cx.run_until_parked();
    let dialog = root.read_with(cx, |root, _| root.dialog.clone());

    click(cx, "settings-category-内存缓存");
    assert!(cx.debug_bounds("总缓存上限").is_some());
    let input = dialog.read_with(cx, |dialog, _| dialog.http_cache_mib.clone());
    cx.update(|window, cx| input.read(cx).focus_handle(cx).focus(window, cx));
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("64");

    let search = dialog.read_with(cx, |dialog, _| dialog.search.clone());
    cx.update(|window, cx| search.read(cx).focus_handle(cx).focus(window, cx));
    cx.simulate_input("disk_cache");
    assert!(cx.debug_bounds("启用磁盘缓存").is_some());
    assert!(cx.debug_bounds("总缓存上限").is_none());
    click(cx, "settings-toggle-启用磁盘缓存");
    assert!(dialog.read_with(cx, |dialog, _| dialog.disk_cache));

    cx.update(|window, cx| search.read(cx).focus_handle(cx).focus(window, cx));
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("no-matching-setting");
    assert!(cx.debug_bounds("启用磁盘缓存").is_none());
    assert!(cx.debug_bounds("window-control-close").is_some());

    click(cx, "settings-category-内存缓存");
    assert!(search.read_with(cx, |search, _| search.value().is_empty()));
    assert_eq!(input.read_with(cx, |input, _| input.value()), "64");
    click(cx, "settings-category-预读策略");
    let expected = PlaybackCacheConfig {
        disk_cache: true,
        http_cache_max_bytes: 64 * BYTES_PER_MIB,
        ..PlaybackCacheConfig::default()
    }
    .normalized();
    assert_eq!(
        root.read_with(cx, |root, _| root.saved.clone()),
        Some(expected)
    );
}

#[gpui::test]
fn cache_directory_is_read_only_and_saving_preserves_its_configuration(cx: &mut TestAppContext) {
    let (root, cx) = settings_window(cx);
    cx.simulate_resize(size(px(1100.0), px(720.0)));
    cx.run_until_parked();
    let dialog = root.read_with(cx, |root, _| root.dialog.clone());
    // A long existing directory must not be truncated by an input character limit.
    let configured = std::env::temp_dir().join("cache-segment/".repeat(24));
    for directory in [None, Some(configured)] {
        let config = PlaybackCacheConfig {
            cache_dir: directory.clone(),
            ..Default::default()
        };
        cx.update(|_, cx| {
            dialog.update(cx, |dialog, cx| {
                *dialog = PlaybackSettingsDialogState::new(&config, cx);
                cx.notify();
            });
        });
        cx.run_until_parked();
        click(cx, "settings-category-磁盘缓存");
        let displayed = dialog.read_with(cx, |dialog, _| dialog.cache_directories.clone());
        assert!(displayed.iter().all(|path| !path.as_os_str().is_empty()));
        if let Some(path) = &directory {
            assert_eq!(displayed, [path.clone(), path.clone()]);
        }
        click(cx, "cache-directory-0");
        cx.simulate_keystrokes("ctrl-a backspace");
        cx.simulate_input("replacement-path");
        assert_eq!(
            dialog.read_with(cx, |dialog, _| dialog.cache_directories.clone()),
            displayed,
        );
        click(cx, "settings-toggle-启用磁盘缓存");
        assert_eq!(
            root.read_with(cx, |root, _| root.saved.as_ref().unwrap().cache_dir.clone()),
            directory,
        );
    }
}

#[gpui::test]
fn content_fills_window_and_scrolls_below_fixed_titlebar_at_supported_window_sizes(
    cx: &mut TestAppContext,
) {
    let (root, cx) = settings_window(cx);
    let dialog = root.read_with(cx, |root, _| root.dialog.clone());

    for (width, height) in [(900.0, 600.0), (1100.0, 720.0), (1600.0, 960.0)] {
        cx.simulate_resize(size(px(width), px(height)));
        cx.run_until_parked();
        click(cx, "settings-category-内存缓存");
        let panel = cx.debug_bounds("playback-settings-panel").unwrap();
        let close = cx.debug_bounds("window-control-close").unwrap();
        let scroll = dialog.read_with(cx, |dialog, _| dialog.scroll_handle.clone());
        assert_eq!(panel.left(), px(0.0));
        assert_eq!(panel.right(), px(width));
        assert_eq!(panel.top(), px(crate::ui::titlebar::APP_TITLEBAR_HEIGHT_PX));
        assert_eq!(panel.bottom(), px(height));
        assert!(close.bottom() <= scroll.bounds().top());
        assert_eq!(scroll.bounds().bottom(), panel.bottom());
        assert_eq!(
            cx.debug_bounds("playback-settings-scrollbar").unwrap(),
            scroll.bounds()
        );
        if height <= 720.0 {
            assert!(scroll.max_offset().y > px(0.0));
        } else {
            assert_eq!(scroll.max_offset().y, px(0.0));
        }
        cx.update(|_, cx| {
            dialog.update(cx, |_, cx| {
                scroll.set_offset(gpui::point(px(0.0), -scroll.max_offset().y));
                cx.notify();
            });
        });
        cx.run_until_parked();
        let last_control = cx.debug_bounds("settings-toggle-共享空闲前向预算").unwrap();
        assert!(last_control.top() >= scroll.bounds().top());
        assert!(last_control.bottom() <= scroll.bounds().bottom());
        assert_eq!(cx.debug_bounds("window-control-close").unwrap(), close);

        click(cx, "settings-category-磁盘缓存");
        assert_eq!(scroll.offset().y, px(0.0));
        let path = cx.debug_bounds("缓存目录").unwrap();
        assert!(path.left() >= scroll.bounds().left());
        assert!(path.right() <= scroll.bounds().right());
    }
}

#[gpui::test]
fn dropdown_selects_by_mouse_and_keyboard_and_dismisses_without_changing_value(
    cx: &mut TestAppContext,
) {
    let (root, cx) = settings_window(cx);
    cx.simulate_resize(size(px(1100.0), px(720.0)));
    cx.run_until_parked();
    let dialog = root.read_with(cx, |root, _| root.dialog.clone());

    click(cx, "settings-category-缓存策略");
    click(cx, "cache-mode-dropdown");
    let trigger = cx.debug_bounds("cache-mode-dropdown").unwrap();
    let menu = cx
        .debug_bounds("cache-mode-dropdown-menu")
        .expect("open dropdown");
    assert_eq!(trigger.size.height, px(28.0));
    assert_eq!(menu.right(), trigger.right());
    assert_eq!(menu.top(), trigger.bottom() + px(4.0));
    click(cx, "cache-mode-enabled");
    assert_eq!(
        dialog.read_with(cx, |dialog, _| dialog.mode),
        PlaybackCacheMode::Enabled
    );
    assert!(cx.debug_bounds("cache-mode-dropdown-menu").is_none());

    click(cx, "cache-mode-dropdown");
    let position = cx.debug_bounds("cache-mode-dropdown").unwrap().center();
    cx.simulate_mouse_down(position, gpui::MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("cache-mode-dropdown-menu").is_none());
    cx.simulate_mouse_up(position, gpui::MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("cache-mode-dropdown-menu").is_none());
    click(cx, "cache-mode-dropdown");
    cx.simulate_keystrokes("down enter");
    assert_eq!(
        dialog.read_with(cx, |dialog, _| dialog.mode),
        PlaybackCacheMode::Disabled
    );
    assert!(cx.debug_bounds("cache-mode-dropdown-menu").is_none());

    // The trigger regains focus after a selection, so arrows can open it again.
    cx.simulate_keystrokes("up");
    settle_menu_frames(cx);
    cx.simulate_keystrokes("home escape");
    assert_eq!(
        dialog.read_with(cx, |dialog, _| dialog.mode),
        PlaybackCacheMode::Disabled
    );
    assert!(cx.debug_bounds("cache-mode-dropdown-menu").is_none());
    click(cx, "cache-mode-dropdown");
    cx.simulate_click(gpui::point(px(20.0), px(20.0)), Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("cache-mode-dropdown-menu").is_none());

    click(cx, "seekable-cache-dropdown");
    click(cx, "cache-mode-dropdown");
    assert!(cx.debug_bounds("seekable-cache-dropdown-menu").is_none());
    assert!(cx.debug_bounds("cache-mode-dropdown-menu").is_some());
    click(cx, "settings-category-内存缓存");
    assert!(cx.debug_bounds("cache-mode-dropdown-menu").is_none());
}

#[gpui::test]
fn dropdown_near_window_bottom_remains_visible_and_selectable(cx: &mut TestAppContext) {
    let (root, cx) = settings_window(cx);
    cx.simulate_resize(size(px(900.0), px(600.0)));
    cx.run_until_parked();
    let dialog = root.read_with(cx, |root, _| root.dialog.clone());
    click(cx, "settings-category-磁盘缓存");
    cx.update(|_, cx| {
        dialog.update(cx, |dialog, cx| {
            dialog
                .scroll_handle
                .set_offset(gpui::point(px(0.0), -dialog.scroll_handle.max_offset().y));
            cx.notify();
        })
    });
    cx.run_until_parked();
    click(cx, "unlink-dropdown");
    let menu = cx
        .debug_bounds("unlink-dropdown-menu")
        .expect("visible menu");
    let trigger = cx.debug_bounds("unlink-dropdown").unwrap();
    assert_eq!(menu.right(), trigger.right());
    assert!(menu.top() >= px(0.0) && menu.bottom() <= px(600.0));
    assert!(menu.left() >= px(0.0) && menu.right() <= px(900.0));
    click(cx, "unlink-never");
    assert_eq!(
        root.read_with(cx, |root, _| root.saved.as_ref().unwrap().unlink_files),
        CacheUnlinkPolicy::Never
    );
}

#[gpui::test]
fn stepper_uses_typed_value_modifiers_and_automatically_saves_edits(cx: &mut TestAppContext) {
    let (root, cx) = settings_window(cx);
    cx.simulate_resize(size(px(1100.0), px(720.0)));
    cx.run_until_parked();
    let dialog = root.read_with(cx, |root, _| root.dialog.clone());
    click(cx, "settings-category-内存缓存");
    let input = dialog.read_with(cx, |dialog, _| dialog.http_cache_mib.clone());
    assert_eq!(
        cx.debug_bounds("http-cache").unwrap().size,
        size(px(120.0), px(28.0))
    );
    assert_eq!(
        cx.debug_bounds("http-cache-input").unwrap().size,
        size(px(64.0), px(28.0))
    );
    cx.update(|window, cx| input.read(cx).focus_handle(cx).focus(window, cx));
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("63");
    click(cx, "http-cache-increment");
    assert_eq!(input.read_with(cx, |input, _| input.value()), "64");
    click(cx, "http-cache-decrement");
    assert_eq!(input.read_with(cx, |input, _| input.value()), "63");
    let plus = cx.debug_bounds("http-cache-increment").unwrap();
    cx.simulate_click(
        plus.center(),
        Modifiers {
            shift: true,
            ..Default::default()
        },
    );
    cx.run_until_parked();
    assert_eq!(input.read_with(cx, |input, _| input.value()), "73");

    click(cx, "settings-category-预读策略");
    let seconds = dialog.read_with(cx, |dialog, _| dialog.cache_secs.clone());
    cx.update(|window, cx| seconds.read(cx).focus_handle(cx).focus(window, cx));
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("0.2");
    let plus = cx.debug_bounds("cache-secs-increment").unwrap();
    cx.simulate_click(
        plus.center(),
        Modifiers {
            alt: true,
            ..Default::default()
        },
    );
    cx.run_until_parked();
    assert_eq!(seconds.read_with(cx, |input, _| input.value()), "0.3");
    click(cx, "cache-secs-decrement");
    click(cx, "cache-secs-decrement");
    assert_eq!(seconds.read_with(cx, |input, _| input.value()), "0");
    let saved = root.read_with(cx, |root, _| root.saved.clone().unwrap());
    assert_eq!(saved.http_cache_max_bytes, 73 * BYTES_PER_MIB);
    assert_eq!(saved.cache_secs, 0.0);
}

#[gpui::test]
fn invalid_numeric_edits_preserve_last_saved_values_and_recover(cx: &mut TestAppContext) {
    let (root, cx) = settings_window(cx);
    let dialog = root.read_with(cx, |root, _| root.dialog.clone());
    let seconds = dialog.read_with(cx, |dialog, _| dialog.cache_secs.clone());
    seconds.update(cx, |input, cx| input.set_value("12.5", cx));
    cx.run_until_parked();
    assert_eq!(root.read_with(cx, |root, _| root.change_count), 1);

    for value in ["", ".", "-", "NaN", "inf", "-1", "1e"] {
        seconds.update(cx, |input, cx| input.set_value(value, cx));
        cx.run_until_parked();
        assert_eq!(root.read_with(cx, |root, _| root.change_count), 1);
        assert_eq!(
            root.read_with(cx, |root, _| root.saved.as_ref().unwrap().cache_secs),
            12.5
        );
    }

    dialog.update(cx, |dialog, cx| {
        dialog.select_color_theme(ColorTheme::Latte, cx);
    });
    cx.run_until_parked();
    assert_eq!(root.read_with(cx, |root, _| root.change_count), 2);
    assert_eq!(
        root.read_with(cx, |root, _| root.saved.as_ref().unwrap().cache_secs),
        12.5
    );
    seconds.update(cx, |input, cx| input.set_value("0", cx));
    cx.run_until_parked();
    assert_eq!(root.read_with(cx, |root, _| root.change_count), 3);
    assert_eq!(
        root.read_with(cx, |root, _| root.saved.as_ref().unwrap().cache_secs),
        0.0
    );

    let ranges = dialog.read_with(cx, |dialog, _| dialog.max_ranges.clone());
    for value in ["", "0", "999"] {
        ranges.update(cx, |input, cx| input.set_value(value, cx));
        cx.run_until_parked();
        assert_eq!(root.read_with(cx, |root, _| root.change_count), 3);
        assert_eq!(
            root.read_with(cx, |root, _| root
                .saved
                .as_ref()
                .unwrap()
                .demuxer_max_ranges),
            PlaybackCacheConfig::default().demuxer_max_ranges
        );
    }
    ranges.update(cx, |input, cx| input.set_value("64", cx));
    cx.run_until_parked();
    assert_eq!(root.read_with(cx, |root, _| root.change_count), 4);
    assert_eq!(
        root.read_with(cx, |root, _| root
            .saved
            .as_ref()
            .unwrap()
            .demuxer_max_ranges),
        64
    );
}

#[gpui::test]
fn autosaving_an_option_preserves_other_fields_at_full_precision(cx: &mut TestAppContext) {
    let config = PlaybackCacheConfig {
        http_cache_chunk_bytes: 64 * 1024,
        http_cache_max_bytes: 33 * BYTES_PER_MIB + 123,
        cache_secs: 12.34567,
        ..Default::default()
    }
    .normalized();
    let (root, cx) = settings_window_with_config(cx, config.clone());
    cx.simulate_resize(size(px(1100.0), px(720.0)));
    cx.run_until_parked();
    click(cx, "settings-category-外观");
    click(cx, "color-theme-dropdown");
    click(cx, "theme-mocha");
    assert_eq!(root.read_with(cx, |root, _| root.change_count), 0);
    click(cx, "color-theme-dropdown");
    click(cx, "theme-latte");
    assert_eq!(
        root.read_with(cx, |root, _| root.saved.clone()),
        Some(config.clone())
    );
    click(cx, "settings-category-磁盘缓存");
    click(cx, "settings-toggle-启用磁盘缓存");
    assert_eq!(
        root.read_with(cx, |root, _| root.saved.clone()),
        Some(PlaybackCacheConfig {
            disk_cache: !config.disk_cache,
            ..config
        })
    );
    click(cx, "window-control-close");
    assert_eq!(root.read_with(cx, |root, _| root.change_count), 2);
}

#[gpui::test]
fn centered_numeric_editor_maps_clicks_to_both_ends_of_the_value(cx: &mut TestAppContext) {
    let (root, cx) = settings_window(cx);
    cx.simulate_resize(size(px(1100.0), px(720.0)));
    cx.run_until_parked();
    click(cx, "settings-category-内存缓存");
    let input = root.read_with(cx, |root, cx| root.dialog.read(cx).http_cache_mib.clone());
    let bounds = cx.debug_bounds("http-cache-input").unwrap();
    cx.simulate_click(
        gpui::point(bounds.left() + px(3.0), bounds.center().y),
        Modifiers::default(),
    );
    cx.simulate_input("1");
    assert_eq!(input.read_with(cx, |input, _| input.value()), "132");
    cx.simulate_click(
        gpui::point(bounds.right() - px(3.0), bounds.center().y),
        Modifiers::default(),
    );
    cx.simulate_input("4");
    assert_eq!(input.read_with(cx, |input, _| input.value()), "1324");
}
