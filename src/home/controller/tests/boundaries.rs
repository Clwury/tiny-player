use super::*;
use crate::home::{
    model::notification::{ActionNotification, NotificationScope},
    resume_actions::controller::{ResumeItemAction, ResumeItemActionResponse},
    search::{controller::SearchIntent, model::SearchPage},
};

fn notification() -> ActionNotification {
    ActionNotification {
        scope: NotificationScope::Home,
        key: "test:mutation".into(),
    }
}

fn favorite(id: &str) -> FavoriteIntent {
    FavoriteIntent {
        item_id: id.into(),
        fallback: None,
        notification: notification(),
    }
}

fn mark(id: &str) -> PlayedIntent {
    PlayedIntent::MarkItem {
        item_id: id.into(),
        notification: notification(),
    }
}

#[test]
fn search_result_checks_mounted_account_before_merging_shared_user_data() {
    let mut home = HomeController::new(identity());
    let request = home
        .dispatch_search(SearchIntent::Submit("movie".into()))
        .request
        .unwrap();
    let mut other = identity();
    other.user_id = Some("other-user".into());
    assert!(
        home.complete_search(
            &request,
            Ok(SearchPage::from_response(items("Rejected", 20))),
            &other
        )
        .is_none()
    );
    assert!(home.search_view().items.is_empty());
    assert!(home.loaded_playback_user_data("movie").is_none());

    let update = home
        .complete_search(
            &request,
            Ok(SearchPage::from_response(items("Accepted", 30))),
            &identity(),
        )
        .unwrap();
    assert!(update.received.is_some());
    assert_eq!(home.search_view().items[0].name, "Accepted");
    assert_eq!(
        home.loaded_playback_user_data("movie")
            .unwrap()
            .playback_position_ticks,
        Some(30)
    );
    assert!(
        home.complete_search(
            &request,
            Ok(SearchPage::from_response(items("Duplicate", 40))),
            &identity()
        )
        .is_none()
    );
    assert_eq!(home.search_view().items[0].name, "Accepted");
}

#[test]
fn favorite_intent_derives_route_and_rolls_back_both_owned_states() {
    let mut home = HomeController::new(identity());
    home.navigation.select_root(HomeRoot::Favorites);
    home.favorites.test_state_mut(VideoItemType::Movie).items = items("Favorite", 10).items;
    let command = home
        .dispatch_favorite(FavoriteIntent {
            fallback: Some(UserItemData {
                is_favorite: true,
                ..Default::default()
            }),
            ..favorite("movie")
        })
        .unwrap();
    assert!(!command.desired);
    assert!(!home.has_favorites());
    assert!(!home.effective_user_data("movie", None).unwrap().is_favorite);
    assert!(home.user_data_pending());

    // Returning to another root does not change the initiating rollback target.
    home.navigation.select_root(HomeRoot::Search);
    let mut other = identity();
    other.user_id = Some("other-user".into());
    assert!(
        home.complete_favorite(&command, Err(anyhow::anyhow!("old account")), &other)
            .is_none()
    );
    assert!(home.favorite_pending());
    let update = home
        .complete_favorite(&command, Err(anyhow::anyhow!("rejected")), &identity())
        .unwrap();
    assert!(update.failure.is_some());
    assert!(!home.user_data_pending());
    assert_eq!(
        home.favorite_section(VideoItemType::Movie).paged.items[0].id,
        "movie"
    );
    assert!(home.effective_user_data("movie", None).is_none());
    assert!(home.user_item_by_id("movie").unwrap().is_favorite());
}

#[test]
fn mutation_conflicts_keep_resume_targets_independent_and_played_exclusive() {
    let mut home = HomeController::new(identity());
    home.feed.state.resume_items = Some(resume());
    home.search.test_state_mut().items = items("Search", 10).items;
    let favorite = home.dispatch_favorite(favorite("movie")).unwrap();
    assert!(
        home.dispatch_resume_action("movie".into(), ResumeItemAction::HideFromResume)
            .is_none()
    );
    let resume = home
        .dispatch_resume_action("other".into(), ResumeItemAction::HideFromResume)
        .unwrap();
    assert!(home.dispatch_played(mark("movie")).is_none());
    assert!(home.dispatch_favorite(self::favorite("physical")).is_none());
    assert!(
        home.complete_resume_action(
            &resume,
            Ok(ResumeItemActionResponse::HiddenFromResume),
            &identity()
        )
        .is_some()
    );
    assert!(home.resume_item_by_id("other").is_none());
    assert!(home.resume_item_by_id("movie").is_some());
    home.complete_favorite(&favorite, Err(anyhow::anyhow!("rejected")), &identity())
        .unwrap();
    let played = home.dispatch_played(mark("movie")).unwrap();
    assert!(
        home.dispatch_resume_action("physical".into(), ResumeItemAction::MarkPlayed)
            .is_none()
    );
    assert!(home.dispatch_favorite(self::favorite("physical")).is_none());
    let completion = home
        .accept_played(played, Err(anyhow::anyhow!("rejected")), &identity())
        .unwrap();
    assert!(!completion.succeeded());
    home.apply_played_completion(completion);
    assert!(!home.user_data_pending());
}

#[test]
fn played_intent_uses_detail_selection_and_effective_user_data() {
    let mut home = HomeController::new(identity());
    assert!(
        home.dispatch_played(PlayedIntent::ToggleDetail {
            whole_series: false
        })
        .is_none()
    );
    let mut detail =
        DetailController::from_user_item(&item("movie", "Movie", 0), identity()).unwrap();
    detail.state.item = Some(
        serde_json::from_value(json!({
            "Id": "movie", "Name": "Movie", "Type": "Movie", "UserData": {"Played": false}
        }))
        .unwrap(),
    );
    home.open_detail(detail);
    home.user_data.overrides.insert(
        "movie".into(),
        UserItemData {
            played: true,
            ..Default::default()
        },
    );
    assert!(
        home.dispatch_played(PlayedIntent::ToggleDetail { whole_series: true })
            .is_none()
    );
    let command = home
        .dispatch_played(PlayedIntent::ToggleDetail {
            whole_series: false,
        })
        .unwrap();
    assert_eq!(command.request.item_id, "movie");
    assert!(!command.request.played);
    assert!(command.request.user_data.unwrap().played);
    assert_eq!(
        command.request.notification.scope,
        NotificationScope::Detail
    );
    assert_eq!(command.request.notification.key, "detail:played");
    assert!(command.request.detail_activation.is_some());
}

#[test]
fn feed_results_validate_account_and_preserve_pending_favorite_overrides() {
    use crate::home::feed::{FeedIntent, FeedResponse, FeedUpdate};

    for latest in [false, true] {
        let mut home = HomeController::new(identity());
        home.feed.state.user_views = Some(
            serde_json::from_value(json!({
                "Items": [{"Id":"library", "Name":"Movies", "CollectionType":"movies"}],
                "TotalRecordCount": 1
            }))
            .unwrap(),
        );
        let request = home
            .dispatch_feed(if latest {
                FeedIntent::LoadLatest
            } else {
                FeedIntent::LoadResume
            })
            .pop()
            .unwrap();
        let revision = home.user_data_request_revision();
        let favorite = home.dispatch_favorite(favorite("movie")).unwrap();
        let response = || {
            if latest {
                let mut item = item("movie", "Latest", 20);
                item.user_data.as_mut().unwrap().is_favorite = false;
                FeedResponse::Latest(Ok(vec![item]))
            } else {
                FeedResponse::Resume(Ok(resume()))
            }
        };
        let mut other = identity();
        other.user_id = Some("other-user".into());
        assert!(matches!(
            home.complete_feed(&request, revision, response(), &other),
            FeedUpdate::Ignored
        ));
        assert!(home.feed_view().resume.is_none());
        assert!(home.latest_row("library").items.is_none());
        let update = home.complete_feed(&request, revision, response(), &identity());
        assert!(matches!(update, FeedUpdate::Resume | FeedUpdate::Latest(_)));
        assert!(home.effective_user_data("movie", None).unwrap().is_favorite);
        assert!(home.favorite_pending());
        assert!(matches!(
            home.complete_feed(&request, revision, response(), &identity()),
            FeedUpdate::Ignored
        ));
        home.complete_favorite(&favorite, Err(anyhow::anyhow!("rejected")), &identity())
            .unwrap();
        assert!(!home.loaded_playback_user_data("movie").unwrap().is_favorite);
    }
}
