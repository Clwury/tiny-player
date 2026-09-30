use super::*;
use crate::{
    player::{HardwareDecodeMode, TrackLanguage},
    settings::{SettingsPersistenceAdapter, memory_budget::MemoryBudget},
};

fn controller(mode: SettingsMode) -> SettingsController {
    SettingsController::new(
        &PlaybackCacheConfig {
            cache_secs: 12.345678901,
            http_cache_max_bytes: 33 * BYTES_PER_MIB + 123,
            disk_cache_max_bytes: 3 * 1024 * BYTES_PER_MIB + 123,
            cache_dir: Some("custom-cache".into()),
            ..Default::default()
        },
        mode,
        ColorTheme::ALL[0],
        PlaybackLanguagePreferences::default(),
    )
}

fn edit(controller: &mut SettingsController, field: NumericSetting, input: &str) -> SettingsChange {
    controller.dispatch(SettingsIntent::EditNumber {
        field,
        input: input.into(),
    })
}

#[test]
fn both_modes_keep_unedited_precision_when_applying_theme_language_and_decoder() {
    for mode in [SettingsMode::Development, SettingsMode::User] {
        let mut controller = controller(mode);
        let original = controller.snapshot();
        let theme = ColorTheme::ALL[1];
        let change = controller.dispatch(SettingsIntent::Theme(theme));
        assert!(change.persist && change.view_changed);
        assert_eq!(change.theme, Some(theme));
        assert!(!controller.dispatch(SettingsIntent::Theme(theme)).persist);
        assert_eq!(controller.snapshot().playback, original.playback);
        let language = TrackLanguage::ALL[1];
        let change = controller.dispatch(SettingsIntent::Language {
            language,
            audio: true,
        });
        assert!(change.persist);
        assert_eq!(change.languages.unwrap().audio, language);
        assert_eq!(controller.snapshot().playback, original.playback);
        controller.dispatch(SettingsIntent::HardwareDecode(HardwareDecodeMode::Off));
        let expected = PlaybackCacheConfig {
            hardware_decode: HardwareDecodeMode::Off,
            ..original.playback
        };
        assert_eq!(controller.snapshot().playback, expected);
    }
}

#[test]
fn invalid_numeric_edits_never_enter_snapshot_and_retry_updates_only_the_edited_field() {
    let mut controller = controller(SettingsMode::Development);
    let original = controller.snapshot();
    for (field, inputs) in [
        (
            NumericSetting::CacheSecs,
            vec!["", ".", "-", "NaN", "inf", "-1", "1e"],
        ),
        (NumericSetting::MaxRanges, vec!["0", "65", "-1", "abc"]),
        (NumericSetting::HttpCacheChunkMib, vec!["0", "17"]),
        (NumericSetting::RangeRequestMib, vec!["0", "129"]),
        (
            NumericSetting::DiskCacheGib,
            vec!["0", "-1", "18446744073709551615"],
        ),
    ] {
        for input in inputs {
            assert!(
                !edit(&mut controller, field, input).persist,
                "{field:?}: {input}"
            );
            assert_eq!(controller.snapshot(), original);
            assert_eq!(
                controller.view_model().validation(field),
                SettingValidation::Invalid
            );
        }
    }
    let change = edit(&mut controller, NumericSetting::CacheSecs, "12.345678901");
    assert!(
        !change.persist && change.view_changed,
        "recovering the saved value only clears validation"
    );
    assert_eq!(
        controller
            .view_model()
            .validation(NumericSetting::CacheSecs),
        SettingValidation::Valid
    );
    assert!(edit(&mut controller, NumericSetting::CacheSecs, "42.987654321").persist);
    assert_eq!(controller.view_model().config.cache_secs, 42.987654321);
    assert_eq!(
        controller.view_model().config.disk_cache_max_bytes,
        original.playback.disk_cache_max_bytes
    );
    assert_eq!(
        controller
            .view_model()
            .validation(NumericSetting::DiskCacheGib),
        SettingValidation::Invalid
    );
}

#[test]
fn validation_projection_tracks_only_the_edited_field_and_notifies_on_transitions() {
    for mode in [SettingsMode::Development, SettingsMode::User] {
        let mut controller = controller(mode);
        let original = controller.snapshot();
        assert!(controller.view_model().editable);
        let field = NumericSetting::DiskCacheGib;
        assert_eq!(
            controller.view_model().validation(field),
            SettingValidation::Valid
        );
        let change = edit(&mut controller, field, "0");
        assert!(change.view_changed && !change.persist);
        let change = edit(&mut controller, field, "invalid");
        assert!(!change.view_changed && !change.persist);
        assert_eq!(controller.snapshot(), original);
        controller.dispatch(SettingsIntent::Category(SettingsCategory::Disk));
        controller.dispatch(SettingsIntent::Search("cache".into()));
        assert_eq!(
            controller.view_model().validation(field),
            SettingValidation::Invalid
        );
        assert_eq!(
            controller
                .view_model()
                .validation(NumericSetting::CacheSecs),
            SettingValidation::Valid
        );
        let change = edit(&mut controller, field, "5");
        assert!(change.view_changed && change.persist);
        assert_eq!(
            controller.view_model().validation(field),
            SettingValidation::Valid
        );
        let change = edit(&mut controller, field, "5");
        assert!(!change.view_changed && !change.persist);
        controller.dispatch(SettingsIntent::Close);
        assert!(!controller.view_model().editable);
        let change = edit(&mut controller, field, "invalid");
        assert!(!change.view_changed && !change.persist);
        assert_eq!(
            controller.view_model().validation(field),
            SettingValidation::Valid
        );
    }
}

#[test]
fn integer_edit_normalizes_dependencies_without_changing_the_requested_value() {
    let mut controller = SettingsController::new(
        &PlaybackCacheConfig {
            http_cache_chunk_bytes: BYTES_PER_MIB,
            http_cache_max_bytes: BYTES_PER_MIB,
            http_cache_range_request_bytes: BYTES_PER_MIB,
            ..Default::default()
        },
        SettingsMode::Development,
        ColorTheme::ALL[0],
        PlaybackLanguagePreferences::default(),
    );
    assert!(edit(&mut controller, NumericSetting::HttpCacheChunkMib, "16").persist);
    let config = controller.view_model().config;
    assert_eq!(config.http_cache_chunk_bytes, 16 * BYTES_PER_MIB);
    assert_eq!(config.http_cache_max_bytes, 16 * BYTES_PER_MIB);
    assert_eq!(config.http_cache_range_request_bytes, 16 * BYTES_PER_MIB);
    let previous = controller.snapshot();
    assert!(!edit(&mut controller, NumericSetting::HttpCacheMib, "1").persist);
    assert_eq!(controller.snapshot(), previous);
}

#[test]
fn navigation_and_close_do_not_save_or_undo_already_applied_settings() {
    for mode in [SettingsMode::User, SettingsMode::Development] {
        let mut controller = controller(mode);
        let original = controller.snapshot();
        assert!(
            !controller
                .dispatch(SettingsIntent::Search("  HTTP 内存  ".into()))
                .persist
        );
        assert_eq!(controller.view_model().query, "HTTP 内存");
        assert!(
            !controller
                .dispatch(SettingsIntent::Category(SettingsCategory::Disk))
                .persist
        );
        assert_eq!(controller.view_model().category, SettingsCategory::Disk);
        assert!(controller.view_model().query.is_empty());
        controller.dispatch(SettingsIntent::Category(SettingsCategory::Readahead));
        assert_eq!(
            controller.view_model().category,
            if mode == SettingsMode::User {
                SettingsCategory::Disk
            } else {
                SettingsCategory::Readahead
            }
        );
        assert_eq!(controller.snapshot(), original);
        assert!(
            controller
                .dispatch(SettingsIntent::Toggle(ToggleSetting::DiskCache))
                .persist
        );
        let changed = controller.snapshot();
        assert!(!controller.dispatch(SettingsIntent::Close).persist);
        assert!(
            !controller
                .dispatch(SettingsIntent::Toggle(ToggleSetting::DiskCache))
                .persist
        );
        assert_eq!(controller.snapshot(), changed);
    }
}

#[test]
fn capacity_and_persistence_adapter_preserve_unrelated_global_and_catalog_data() {
    let mut controller = controller(SettingsMode::User);
    let original = controller.snapshot();
    controller.dispatch(SettingsIntent::MemoryBudget(Some(MemoryBudget::Gib1)));
    assert_eq!(
        controller.view_model().config.cache_secs,
        original.playback.cache_secs
    );
    assert_eq!(
        controller.view_model().config.disk_cache_max_bytes,
        original.playback.disk_cache_max_bytes
    );
    assert_eq!(
        controller.view_model().config.cache_dir,
        original.playback.cache_dir
    );
    let mut cache = crate::storage::ServerCache::empty();
    cache.playback_volume.level = 0.47;
    cache.set_window_size(1234, 789);
    cache.auto_start_server_id = Some("unchanged".into());
    let (mut global, catalog) = crate::config::GlobalConfig::split(cache);
    SettingsPersistenceAdapter::apply(controller.snapshot(), &mut global);
    let saved = global.snapshot(&catalog);
    assert_eq!(saved.playback, controller.snapshot().playback);
    assert_eq!(saved.playback_volume.level, 0.47);
    assert_eq!(saved.window_size().unwrap().width, 1234);
    assert_eq!(saved.auto_start_server_id.as_deref(), Some("unchanged"));
}

#[test]
fn search_selector_matches_all_words_across_metadata_and_restores_category_filter() {
    let mut controller = controller(SettingsMode::Development);
    let item = SettingDescriptor {
        category: SettingsCategory::Memory,
        section: "共享预算",
        title: "HTTP 内存缓存",
        description: "缓存网络媒体数据",
        keywords: "http_cache_max_bytes transport",
    };
    assert!(!controller.view_model().includes(&item));
    controller.dispatch(SettingsIntent::Search(" Http   TRANSPORT 内存 ".into()));
    assert!(controller.view_model().includes(&item));
    controller.dispatch(SettingsIntent::Search("HTTP 不存在的选项".into()));
    assert!(!controller.view_model().includes(&item));
    controller.dispatch(SettingsIntent::Category(SettingsCategory::Memory));
    assert!(controller.view_model().includes(&item));
    assert!(controller.view_model().query.is_empty());
}

#[test]
fn theme_fallback_result_is_the_value_persisted_and_exposed_to_the_view() {
    let mut controller = controller(SettingsMode::User);
    let previous = controller.snapshot().playback;
    let change = controller.dispatch(SettingsIntent::Theme(ColorTheme::Frappe));
    assert!(change.persist);
    assert_eq!(change.theme, Some(ColorTheme::Frappe));
    controller.finish_theme_selection(ColorTheme::Mocha);
    assert_eq!(controller.view_model().color_theme, ColorTheme::Mocha);
    assert_eq!(controller.snapshot().color_theme, ColorTheme::Mocha);
    assert_eq!(controller.snapshot().playback, previous);
    controller.dispatch(SettingsIntent::Close);
    controller.finish_theme_selection(ColorTheme::Frappe);
    assert_eq!(controller.snapshot().color_theme, ColorTheme::Mocha);
}
