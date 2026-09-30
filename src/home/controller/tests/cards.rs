use super::*;

#[test]
fn card_projections_merge_shared_user_data_without_mutating_media_records() {
    let mut home = HomeController::new(identity());
    let metadata = json!({
        "Id":"episode", "Name":"Third", "Type":"Episode", "SeriesName":"Series",
        "ParentIndexNumber":1, "IndexNumber":3, "CommunityRating":8.75,
        "UserData":{"IsFavorite":false,"Played":false,"PlayedPercentage":10,"UnplayedItemCount":3}
    });
    let item: UserItem = serde_json::from_value(metadata.clone()).unwrap();
    let resume = serde_json::from_value(metadata.clone()).unwrap();
    let episode = serde_json::from_value(metadata).unwrap();
    home.user_data.overrides.insert(
        "episode".into(),
        UserItemData {
            is_favorite: true,
            played: true,
            played_percentage: Some(75.0),
            unplayed_item_count: Some(2),
            ..Default::default()
        },
    );
    let poster = home.user_item_card_vm(&item, true);
    assert_eq!(poster.badges.rating.as_deref(), Some("8.8"));
    assert_eq!(poster.badges.unplayed_count, Some(2));
    assert!(poster.badges.favorite);
    let favorites_poster = home.user_item_card_vm(&item, false);
    assert!(!favorites_poster.badges.favorite);
    assert_eq!(favorites_poster.badges.unplayed_count, Some(2));
    let resume = home.resume_card_vm(&resume);
    assert_eq!(
        (resume.title.as_str(), resume.subtitle.as_deref()),
        ("Series", Some("S1E3: Third"))
    );
    assert!(resume.favorite);
    assert_eq!(resume.played_fraction, Some(0.75));
    let compact = home.user_episode_card_vm(&item);
    assert_eq!(compact.subtitle, "S01E03 · Third");
    assert!(compact.played);
    assert_eq!(compact.played_fraction, Some(0.75));
    let selected = home.episode_card_vm(&episode, true);
    assert!(selected.selected && selected.played);
    assert_eq!(selected.played_fraction, Some(0.75));
    let unselected = home.episode_card_vm(&episode, false);
    assert!(unselected.played && !unselected.selected);
    assert!(unselected.played_fraction.is_none());
    assert_eq!(
        item.user_data.as_ref().unwrap().played_percentage,
        Some(10.0)
    );
    assert!(!item.user_data.as_ref().unwrap().is_favorite);
    assert!(!episode.user_data.as_ref().unwrap().played);
    home.user_data.overrides.clear();
    assert!(!home.user_item_card_vm(&item, true).badges.favorite);
    assert_eq!(home.user_episode_card_vm(&item).played_fraction, Some(0.1));
}
