use super::*;
use crate::{
    emby::{ResumeItems, UserItemData},
    home::test_support::content as page,
};

#[gpui::test]
fn stale_home_failures_never_notify_or_finish_the_replacement(cx: &mut gpui::TestAppContext) {
    let page = page(cx);
    page.update(cx, |page, cx| {
        let old = page
            .controller
            .dispatch_feed(FeedIntent::LoadViews)
            .pop()
            .unwrap();
        let requests = page.controller.dispatch_feed(FeedIntent::Refresh);
        let new = requests
            .into_iter()
            .find(|request| request.kind == FeedRequestKind::Views)
            .unwrap();
        page.finish_feed(
            old,
            0,
            FeedResponse::Views(Err(anyhow::anyhow!("late"))),
            cx,
        );
        assert!(
            page.controller
                .test_state()
                .feed
                .state
                .views_load
                .is_loading()
        );
        assert!(!page.has_visible_notifications());
        page.finish_feed(
            new,
            0,
            FeedResponse::Views(Err(anyhow::anyhow!("current"))),
            cx,
        );
        assert!(
            !page
                .controller
                .test_state()
                .feed
                .state
                .views_load
                .is_loading()
        );
        assert!(page.has_visible_notifications());
    });
}

#[gpui::test]
fn resume_result_keeps_newer_optimistic_data_and_ignores_another_workspace(
    cx: &mut gpui::TestAppContext,
) {
    let page = page(cx);
    page.update(cx, |page, cx| {
        let request = page.controller.dispatch_feed(FeedIntent::LoadResume).pop().unwrap();
        page.controller.test_state_mut().user_data.bump("movie");
        page.controller.test_state_mut().user_data.overrides.insert("movie".into(), UserItemData { is_favorite: true, ..Default::default() });
        let items: ResumeItems = serde_json::from_value(serde_json::json!({
            "Items": [{"Id":"movie", "Name":"Movie", "Type":"Movie", "UserData":{"IsFavorite":false}}], "TotalRecordCount":1
        })).unwrap();
        page.finish_feed(request, 0, FeedResponse::Resume(Ok(items)), cx);
        assert!(page.controller.test_state().user_data.overrides["movie"].is_favorite);
        let mut other = crate::home::feed::FeedController::new(WorkspaceIdentity {
            user_id: Some("other".into()), ..page.request_identity()
        });
        let wrong = other.dispatch(FeedIntent::LoadResume).pop().unwrap();
        page.finish_feed(wrong, 1, FeedResponse::Resume(Err(anyhow::anyhow!("wrong user"))), cx);
        assert!(page.controller.test_state().feed.state.resume_items_failed.is_none());
        assert!(!page.has_visible_notifications());
        assert_eq!(page.controller.test_state().feed.state.resume_items.as_ref().unwrap().items.len(), 1);
    });
}

#[gpui::test]
fn cancelling_cached_followups_prevents_delayed_network_start(cx: &mut gpui::TestAppContext) {
    let page = page(cx);
    page.update(cx, |page, cx| {
        page.schedule_cached_home_followup(true, cx);
        page.schedule_cached_home_followup(false, cx);
    });
    cx.run_until_parked();
    page.update(cx, |page, _| page.feed_effects.cancel_refresh());
    cx.executor().advance_clock(HOME_CACHED_IMAGE_ENSURE_DELAY);
    cx.run_until_parked();
    page.read_with(cx, |page, _| {
        assert_eq!(
            page.controller.test_state().feed.state.views_load,
            crate::home::LoadState::Idle
        );
        assert_eq!(
            page.controller.test_state().feed.state.resume_load,
            crate::home::LoadState::Idle
        );
        assert!(!page.has_visible_notifications());
    });
}
