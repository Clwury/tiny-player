use super::*;
use crate::home::test_support::content as page;
use std::time::Duration;

#[gpui::test]
fn favorite_people_share_detail_portrait_cache_and_skip_missing_images(
    cx: &mut gpui::TestAppContext,
) {
    use crate::{
        emby::{MediaItem, MediaPerson},
        images::test_support::FakeItemImages,
    };
    let page = page(cx);
    let repository = Arc::new(FakeItemImages::default());
    let items: UserItems = serde_json::from_value(serde_json::json!({
        "Items":[
            {"Id":"person", "Name":"Actor", "Type":"Person", "ImageTags":{"Primary":"portrait", "Thumb":"thumb"}},
            {"Id":"no-image", "Name":"Missing", "Type":"Person"}
        ], "TotalRecordCount":2
    })).unwrap();
    let detail: MediaItem = serde_json::from_value(serde_json::json!({
        "Id":"movie", "Name":"Movie", "Type":"Movie",
        "People":[{"Id":"person", "Name":"Actor", "PrimaryImageTag":"portrait"}]
    }))
    .unwrap();
    page.update(cx, |page, cx| {
        page.image_repository = repository.clone();
        page.ensure_favorite_items_images(&items, cx);
        page.ensure_series_media_item_images(&detail, cx);
    });
    cx.run_until_parked();
    let requests = repository.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].request.item_id, "person");
    assert_eq!(requests[0].request.image_type, EmbyImageType::Primary);
    assert_eq!(requests[0].request.tag.as_deref(), Some("portrait"));
    assert_eq!(requests[0].request.max_width, Some(320));
    let person: &MediaPerson = &detail.people.as_ref().unwrap()[0];
    page.read_with(cx, |page, _| {
        let path = page.image_path_for_favorite_person(&items.items[0]);
        assert!(path.is_some());
        assert_eq!(path, page.image_path_for_person_primary(person));
        assert!(
            page.image_path_for_favorite_person(&items.items[1])
                .is_none()
        );
    });
}

#[gpui::test]
fn image_runner_drains_the_queue_through_the_port_and_only_notifies_ready_paths(
    cx: &mut gpui::TestAppContext,
) {
    use crate::images::{controller::ImageController, test_support::FakeItemImages};
    let page = page(cx);
    let repository = Arc::new(FakeItemImages {
        failed: ["one".into()].into(),
        ..Default::default()
    });
    let notifications = std::rc::Rc::new(std::cell::Cell::new(0));
    let observed = notifications.clone();
    cx.update(|cx| {
        cx.observe(&page, move |_, _| observed.set(observed.get() + 1))
            .detach()
    });
    cx.run_until_parked();
    page.update(cx, |page, cx| {
        page.images =
            ImageController::with_limits(page.request_identity(), 1, Duration::from_secs(30), 3);
        page.image_repository = repository.clone();
        for id in ["one", "one", "two", "three"] {
            page.ensure_image(
                EmbyImageRequest::primary(id, Some("tag".into())).with_max_width(640),
                cx,
            );
        }
        assert_eq!(page.image_effects.len(), 1);
    });
    cx.run_until_parked();
    assert_eq!(notifications.get(), 2);
    let requests = repository.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|image| image.request.item_id.as_str())
            .collect::<Vec<_>>(),
        ["one", "two", "three"]
    );
    assert!(requests.iter().all(
        |image| image.key.max_width == Some(640) && image.key.quality == ImageQuality::DEFAULT
    ));
    page.read_with(cx, |page, _| {
        assert!(page.image_effects.is_empty());
        assert!(
            page.image_path_for_primary_image("one", Some("tag"))
                .is_none()
        );
        assert!(
            page.image_path_for_primary_image("two", Some("tag"))
                .is_some()
        );
        assert!(
            page.image_path_for_primary_image("three", Some("tag"))
                .is_some()
        );
        assert!(!page.has_visible_notifications());
    });
}

#[gpui::test]
fn old_image_delivery_keeps_the_current_handle_and_release_cancels_pending_work(
    cx: &mut gpui::TestAppContext,
) {
    use crate::images::controller::ImageController;
    use std::sync::atomic::{AtomicBool, Ordering};
    let page = page(cx);
    let completed = Arc::new(AtomicBool::new(false));
    page.update(cx, |page, cx| {
        let request = EmbyImageRequest::primary("one", Some("tag".into())).with_max_width(640);
        let mut previous = ImageController::new(page.request_identity());
        previous.ensure_image(request.clone(), Instant::now());
        let old = previous.start_queued_jobs().pop().unwrap();
        page.images.ensure_image(request.clone(), Instant::now());
        let current = page.images.start_queued_jobs().pop().unwrap();
        let key = current.image.key.clone();
        let executor = cx.background_executor().clone();
        let completed = completed.clone();
        let task = cx.background_spawn(async move {
            executor.timer(Duration::from_secs(1)).await;
            completed.store(true, Ordering::SeqCst);
            Ok(std::path::PathBuf::from("/tmp/current.png"))
        });
        page.await_item_image(current, task, cx);
        page.finish_item_image(old.clone(), Ok("/tmp/old.png".into()), cx);
        page.finish_item_image(old, Err(anyhow::anyhow!("late failure")), cx);
        assert!(page.images.path_for_request(&request).is_none());
        assert!(page.images.failure_for_request(&request).is_none());
        assert!(page.image_effects.contains_key(&key));
        assert!(!page.has_visible_notifications());
    });
    cx.run_until_parked();
    cx.update(|_| drop(page));
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert!(!completed.load(Ordering::SeqCst));
}

#[test]
fn favorite_episodes_request_the_same_primary_cover_size_as_detail_cards() {
    let item: UserItem = serde_json::from_value(serde_json::json!({
        "Id": "episode-1", "Name": "Episode", "Type": "Episode",
        "ImageTags": { "Primary": "primary-tag", "Thumb": "thumb-tag" },
        "BackdropImageTags": ["backdrop-tag"]
    }))
    .unwrap();
    let request = favorite_episode_image_request(&item);
    assert_eq!(request.item_id, "episode-1");
    assert_eq!(request.image_type, EmbyImageType::Primary);
    assert_eq!(request.tag.as_deref(), Some("primary-tag"));
    assert_eq!(request.max_width, Some(640));
}
