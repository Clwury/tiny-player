use super::*;

#[test]
fn favorite_person_cards_only_project_names_even_when_video_metadata_is_present() {
    for (name, expected) in [("Actor", "Actor"), ("  ", "未知人员")] {
        let item: UserItem = serde_json::from_value(serde_json::json!({
            "Id":"person", "Name":name, "Type":"Person", "ProductionYear":2024,
            "CommunityRating":9.0, "UserData":{"IsFavorite":true}
        }))
        .unwrap();
        let card = PersonCardVm::from(&item);
        assert_eq!(card.name, expected);
        assert!(card.role.is_none());
        assert!(card.kind.is_none());
    }
}

#[test]
fn all_progress_projections_reject_nonfinite_values_and_clamp_valid_percentages() {
    let metadata = serde_json::json!({"Id":"episode","Name":"Episode","Type":"Episode"});
    let item = serde_json::from_value(metadata.clone()).unwrap();
    let resume = serde_json::from_value(metadata.clone()).unwrap();
    let episode = serde_json::from_value(metadata).unwrap();
    for (percentage, expected) in [
        (f64::NAN, None),
        (f64::INFINITY, None),
        (f64::NEG_INFINITY, None),
        (-5.0, Some(0.0)),
        (150.0, Some(1.0)),
        (25.0, Some(0.25)),
    ] {
        let data = UserItemData {
            played_percentage: Some(percentage),
            ..Default::default()
        };
        assert_eq!(
            ResumeCardVm::new(&resume, Some(&data)).played_fraction,
            expected
        );
        assert_eq!(
            UserEpisodeCardVm::new(&item, Some(&data)).played_fraction,
            expected
        );
        assert_eq!(
            EpisodeCardVm::new(&episode, Some(&data), true).played_fraction,
            expected
        );
        assert!(
            EpisodeCardVm::new(&episode, Some(&data), false)
                .played_fraction
                .is_none()
        );
    }
}

#[test]
fn compact_episode_titles_and_zero_badges_preserve_missing_metadata_fallbacks() {
    let mut item: UserItem = serde_json::from_value(serde_json::json!({
        "Id":"episode", "Name":"Special", "Type":"Episode", "SeriesName":"  ", "IndexNumber":3
    }))
    .unwrap();
    let view = UserEpisodeCardVm::new(&item, None);
    assert_eq!(
        (view.title.as_str(), view.subtitle.as_str()),
        ("Special", "E03")
    );
    item.index_number = None;
    assert_eq!(UserEpisodeCardVm::new(&item, None).subtitle, "单集");
    item.series_name = Some("Series".into());
    let view = UserEpisodeCardVm::new(&item, None);
    assert_eq!(
        (view.title.as_str(), view.subtitle.as_str()),
        ("Series", "Special")
    );
    let data = UserItemData {
        unplayed_item_count: Some(0),
        is_favorite: true,
        ..Default::default()
    };
    let view = UserItemCardVm::new(&item, Some(&data), true);
    assert!(view.badges.unplayed_count.is_none() && view.badges.favorite);
    let person: MediaPerson =
        serde_json::from_value(serde_json::json!({"Name":"  ", "Role":" ", "Type":" "})).unwrap();
    let view = PersonCardVm::from(&person);
    assert_eq!(
        (
            view.name.as_str(),
            view.role.as_deref(),
            view.kind.as_deref()
        ),
        ("未知人员", Some("暂无角色"), Some("未知类型"))
    );
}

fn resume_item(item_type: &str, name: &str) -> ResumeItem {
    ResumeItem {
        id: "item-1".to_string(),
        name: name.to_string(),
        item_type: Some(item_type.to_string()),
        parent_id: None,
        series_name: None,
        series_id: None,
        parent_index_number: None,
        index_number: None,
        production_year: None,
        image_tags: None,
        backdrop_image_tags: None,
        parent_backdrop_item_id: None,
        parent_backdrop_image_tags: None,
        user_data: None,
    }
}

#[test]
fn formats_community_rating_badge_text() {
    assert_eq!(format_community_rating(8.0), "8");
    assert_eq!(format_community_rating(8.74), "8.7");
    assert_eq!(format_community_rating(8.75), "8.8");
}

#[test]
fn compacts_indented_paragraphs_before_episode_line_clamp() {
    let overview = "　　改编自飞卢小说网同名小说。\n\n　　顾长歌穿越到玄幻世界，发现自己成了注定被“天命之子”击败的“天命大反派”顾长歌，为求自保并逆天改命，他被迫利用系统和对“爽文套路”的先知，反向算计、掠夺并打压各位“天命之子”，从而获得奖励，一步步走上巅峰。";

    assert_eq!(
        compact_episode_overview(Some(overview)).as_deref(),
        Some(
            "改编自飞卢小说网同名小说。 顾长歌穿越到玄幻世界，发现自己成了注定被“天命之子”击败的“天命大反派”顾长歌，为求自保并逆天改命，他被迫利用系统和对“爽文套路”的先知，反向算计、掠夺并打压各位“天命之子”，从而获得奖励，一步步走上巅峰。"
        )
    );
    assert_eq!(compact_episode_overview(Some(" \n　\t")), None);
}

#[test]
fn formats_episode_resume_card_text() {
    let mut item = resume_item("Episode", "第三集");
    item.series_name = Some("示例剧集".to_string());
    item.parent_index_number = Some(1);
    item.index_number = Some(3);

    let (title, subtitle) = resume_item_card_text(&item);

    assert_eq!(title, "示例剧集");
    assert_eq!(subtitle.as_deref(), Some("S1E3: 第三集"));
}

#[test]
fn falls_back_for_incomplete_episode_resume_card_text() {
    let item = resume_item("Episode", "特别篇");

    let (title, subtitle) = resume_item_card_text(&item);

    assert_eq!(title, "特别篇");
    assert_eq!(subtitle.as_deref(), Some("特别篇"));
}

#[test]
fn formats_movie_resume_card_text() {
    let mut item = resume_item("Movie", "示例电影");
    item.production_year = Some(2024);

    let (title, subtitle) = resume_item_card_text(&item);

    assert_eq!(title, "示例电影");
    assert_eq!(subtitle.as_deref(), Some("2024"));
}

#[test]
fn omits_missing_movie_resume_card_subtitle() {
    let item = resume_item("Movie", "无年份电影");

    let (title, subtitle) = resume_item_card_text(&item);

    assert_eq!(title, "无年份电影");
    assert!(subtitle.is_none());
}
