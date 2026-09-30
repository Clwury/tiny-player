use super::*;
use anyhow::anyhow;

fn identity() -> WorkspaceIdentity {
    WorkspaceIdentity {
        local_server_id: "local".into(),
        remote_server_id: Some("remote".into()),
        user_id: Some("user".into()),
    }
}
fn request(id: &str) -> EmbyImageRequest {
    EmbyImageRequest::primary(id, Some(format!("tag-{id}"))).with_max_width(640)
}
fn path(id: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(format!("/tmp/{id}.png")))
}

#[test]
fn dedupes_queued_in_flight_and_completed_jobs_without_any_io() {
    let now = Instant::now();
    let mut images = ImageController::new(identity());
    images.ensure_image(request("one"), now);
    images.ensure_image(request("one"), now);
    let jobs = images.start_queued_jobs();
    assert_eq!(jobs.len(), 1);
    images.ensure_image(request("one"), now);
    assert!(images.start_queued_jobs().is_empty());
    assert_eq!(
        images.finish_job(&jobs[0], path("one"), &identity(), now),
        ImageUpdate::Ready
    );
    images.ensure_image(request("one"), now);
    assert!(images.start_queued_jobs().is_empty());
    assert_eq!(
        images.path_for_request(&request("one")).as_deref(),
        Some(Path::new("/tmp/one.png"))
    );
}

#[test]
fn borrowed_source_lookup_reuses_the_same_path_and_distinguishes_image_parameters() {
    let now = Instant::now();
    let mut images = ImageController::new(identity());
    let original = request("one");
    images.ensure_image(original.clone(), now);
    let command = images.start_queued_jobs().pop().unwrap();
    images.finish_job(&command, path("one"), &identity(), now);
    let path = images.path_for_request(&original).unwrap();
    let again = images
        .path_for_source(
            "one",
            EmbyImageType::Primary,
            Some(" tag-one "),
            Some(640),
            ImageQuality::DEFAULT,
        )
        .unwrap();
    assert!(Arc::ptr_eq(&path, &again));
    for changed in [
        original.clone().with_max_width(800),
        original.clone().with_tag(Some("new-tag".into())),
        EmbyImageRequest::new("one", EmbyImageType::Logo)
            .with_tag(Some("tag-one".into()))
            .with_max_width(640),
        request("other"),
    ] {
        assert!(images.path_for_request(&changed).is_none());
        images.ensure_image(changed, now);
    }
    assert_eq!(images.start_queued_jobs().len(), 4);
}

#[test]
fn only_accepted_completion_releases_a_concurrency_slot() {
    let now = Instant::now();
    let mut images = ImageController::with_limits(identity(), 2, Duration::ZERO, 3);
    for id in ["one", "two", "three"] {
        images.ensure_image(request(id), now);
    }
    let jobs = images.start_queued_jobs();
    assert_eq!(jobs.len(), 2);
    assert!(images.start_queued_jobs().is_empty());
    let foreign = WorkspaceIdentity {
        user_id: Some("other".into()),
        ..identity()
    };
    assert_eq!(
        images.finish_job(&jobs[0], path("foreign"), &foreign, now),
        ImageUpdate::Ignored
    );
    assert!(images.start_queued_jobs().is_empty());
    assert!(images.path_for_request(&request("one")).is_none());
    assert_eq!(
        images.finish_job(&jobs[0], Err(anyhow!("offline")), &identity(), now),
        ImageUpdate::Failed
    );
    let next = images.start_queued_jobs();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].image.request.item_id, "three");
    assert_eq!(
        images.finish_job(&jobs[0], path("duplicate"), &identity(), now),
        ImageUpdate::Ignored
    );
    assert!(images.path_for_request(&request("one")).is_none());
}

#[test]
fn retry_obeys_exact_backoff_and_attempt_limit_without_sleeping() {
    let started = Instant::now();
    let backoff = Duration::from_secs(30);
    let mut images = ImageController::with_limits(identity(), 1, backoff, 3);
    for attempt in 0..3 {
        let now = started + backoff * attempt;
        images.ensure_image(request("retry"), now);
        let command = images.start_queued_jobs().pop().unwrap();
        assert_eq!(
            images.finish_job(&command, Err(anyhow!("offline")), &identity(), now),
            ImageUpdate::Failed
        );
        let failure = images.failure_for_request(&request("retry")).unwrap();
        assert_eq!(failure.attempts, attempt as usize + 1);
        assert_eq!(failure.message, "offline");
        images.ensure_image(request("retry"), now + backoff - Duration::from_nanos(1));
        assert!(images.start_queued_jobs().is_empty());
    }
    images.ensure_image(request("retry"), started + backoff * 100);
    assert!(images.start_queued_jobs().is_empty());
}

#[test]
fn old_attempt_and_replaced_owner_cannot_commit_or_fail_a_new_attempt() {
    let now = Instant::now();
    let mut images = ImageController::with_limits(identity(), 1, Duration::ZERO, 3);
    images.ensure_image(request("one"), now);
    let old = images.start_queued_jobs().pop().unwrap();
    images.finish_job(&old, Err(anyhow!("retry")), &identity(), now);
    images.ensure_image(request("one"), now);
    let current = images.start_queued_jobs().pop().unwrap();
    assert_eq!(
        images.finish_job(&old, path("old"), &identity(), now),
        ImageUpdate::Ignored
    );
    assert_eq!(
        images.finish_job(&old, Err(anyhow!("old failure")), &identity(), now),
        ImageUpdate::Ignored
    );
    assert_eq!(
        images
            .failure_for_request(&request("one"))
            .unwrap()
            .attempts,
        1
    );
    images.finish_job(&current, path("current"), &identity(), now);
    assert!(images.failure_for_request(&request("one")).is_none());
    assert_eq!(
        images.path_for_request(&request("one")).as_deref(),
        Some(Path::new("/tmp/current.png"))
    );

    let mut replacement = ImageController::new(identity());
    replacement.ensure_image(request("one"), now);
    let new = replacement.start_queued_jobs().pop().unwrap();
    assert_eq!(
        replacement.finish_job(&current, path("previous page"), &identity(), now),
        ImageUpdate::Ignored
    );
    assert_eq!(
        replacement.finish_job(&new, path("new page"), &identity(), now),
        ImageUpdate::Ready
    );
}

#[test]
fn untagged_requests_do_not_schedule_and_tokens_cannot_cross_image_keys() {
    let now = Instant::now();
    let mut images = ImageController::new(identity());
    for tag in [None, Some("".into()), Some("  ".into())] {
        images.ensure_image(EmbyImageRequest::primary("one", tag), now);
    }
    assert!(images.start_queued_jobs().is_empty());
    images.ensure_image(request("one"), now);
    images.ensure_image(request("two"), now);
    let jobs = images.start_queued_jobs();
    let wrong = ItemImageCommand {
        image: jobs[1].image.clone(),
        token: jobs[0].token.clone(),
    };
    assert_eq!(
        images.finish_job(&wrong, path("wrong"), &identity(), now),
        ImageUpdate::Ignored
    );
    assert_eq!(
        images.finish_job(&jobs[1], path("two"), &identity(), now),
        ImageUpdate::Ready
    );
    assert_eq!(
        images.finish_job(&jobs[0], path("one"), &identity(), now),
        ImageUpdate::Ready
    );
}
