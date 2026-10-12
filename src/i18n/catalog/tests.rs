use super::*;
use std::collections::BTreeSet;

#[test]
fn catalogs_preserve_every_formatted_field_and_cover_the_same_messages() {
    let english = catalog(AppLanguage::English);
    let traditional = catalog(AppLanguage::ChineseTraditional);
    assert_eq!(
        english.exact.keys().copied().collect::<BTreeSet<_>>(),
        traditional.exact.keys().copied().collect::<BTreeSet<_>>()
    );
    for language in AppLanguage::ALL {
        for (source, target) in &catalog(language).templates {
            let fields = |template: &Template| {
                template
                    .segments
                    .iter()
                    .filter_map(|segment| match segment {
                        Segment::Field(field) => Some(field.clone()),
                        Segment::Text(_) => None,
                    })
                    .collect::<BTreeSet<_>>()
            };
            assert_eq!(fields(source), fields(target), "{:?}", source.segments);
        }
    }
}

#[test]
fn interface_labels_counts_durations_and_nested_errors_follow_the_selected_language() {
    for (source, english, traditional) in [
        ("常规", "General", "常規"),
        (
            "应用界面语言",
            "Application interface language",
            "應用介面語言",
        ),
        ("剧集列表 · 12 集", "Episodes · 12", "劇集列表 · 12 集"),
        ("共 42 项", "42 items", "共 42 項"),
        ("继续 1:05:09", "Resume 1:05:09", "繼續 1:05:09"),
        ("2小时12分钟", "2h 12m", "2小時12分鐘"),
        (
            "音轨语言 · 字幕语言",
            "Audio language · Subtitle language",
            "音軌語言 · 字幕語言",
        ),
        (
            "切换剧集失败：目标单集没有可用视频源",
            "Failed to switch episode: Target episode has no playable video source",
            "切換劇集失敗：目標單集沒有可用視訊來源",
        ),
        (
            "切换剧集失败：目标单集不存在；恢复当前播放失败：目标单集视频源缺少 ID",
            "Failed to switch episode: Target episode does not exist; Failed to restore current playback: Target episode video source has no ID",
            "切換劇集失敗：目標單集不存在；恢復目前播放失敗：目標單集視訊來源缺少 ID",
        ),
        (
            "观看状态已更新，刷新失败（分集列表：加载媒体详情失败；当前分集详情：目标单集不存在）",
            "Watched status updated, but failed to refresh (Episodes: Failed to load media details; Current episode details: Target episode does not exist)",
            "觀看狀態已更新，刷新失敗（分集列表：載入媒體詳情失敗；當前分集詳情：目標單集不存在）",
        ),
    ] {
        assert_eq!(translate(AppLanguage::ChineseSimplified, source), source);
        assert_eq!(translate(AppLanguage::English, source), english);
        assert_eq!(
            translate(AppLanguage::ChineseTraditional, source),
            traditional
        );
    }
}

#[test]
fn translated_templates_preserve_content_paths_and_unknown_text() {
    for language in AppLanguage::ALL {
        let content = "自定义影片标题与服务器简介";
        assert_eq!(translate(language, content), content);
    }
    assert_eq!(
        translate(AppLanguage::English, "读取首页缓存失败：/tmp/中文失败"),
        "Failed to read Home cache: /tmp/中文失败"
    );
    assert_eq!(
        translate(
            AppLanguage::English,
            "读取首页缓存失败：/tmp/中文；目标单集不存在"
        ),
        "Failed to read Home cache: /tmp/中文；目标单集不存在"
    );
    assert_eq!(
        translate(AppLanguage::English, "查看收藏类型"),
        "View 收藏 titles"
    );
    assert_eq!(translate(AppLanguage::English, "视频 7"), "Video 7");
    assert_eq!(translate(AppLanguage::ChineseSimplified, "Off"), "关闭");
}
