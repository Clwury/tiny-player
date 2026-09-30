use super::{controller::QueueController, model::*, test_support::*};
use crate::{effects::WorkspaceIdentity, player::PlaybackStateUpdate};

#[test]
fn manual_failure_resumes_only_previously_running_playback() {
    for (paused, ended, should_resume) in [
        (false, false, true),
        (true, false, false),
        (false, true, false),
    ] {
        let mut controller = QueueController::new(queue(), identity());
        let command = controller
            .begin(QueueAction::Next, false, paused, ended)
            .unwrap();
        assert_eq!(command.pause_current, should_resume);
        assert!(controller.view_model().loading);
        assert!(!controller.view_model().can_next());
        assert!(!controller.view_model().can_previous());
        assert!(
            controller
                .begin(QueueAction::Previous, false, paused, ended)
                .is_none()
        );
        let Some(QueueSwitchUpdate::Failed(recovery)) =
            controller.complete(command, Err(anyhow::anyhow!("offline")), &identity())
        else {
            panic!("expected recovery");
        };
        assert_eq!(recovery.resume_current, should_resume);
        assert!(!recovery.publish_terminal_update);
        assert!(!controller.view_model().loading);
        assert_eq!(
            controller.view_model().error,
            Some("切换下一集失败：offline")
        );
        assert_eq!(controller.queue().current_index, 1);
    }
}

#[test]
fn automatic_failure_publishes_terminal_update_exactly_once() {
    let mut controller = QueueController::new(queue(), identity());
    let command = controller
        .begin(QueueAction::Next, true, false, true)
        .unwrap();
    assert!(!command.pause_current);
    let Some(QueueSwitchUpdate::Failed(recovery)) = controller.complete(
        command.clone(),
        Err(anyhow::anyhow!("offline")),
        &identity(),
    ) else {
        panic!("expected terminal recovery");
    };
    assert!(recovery.publish_terminal_update);
    assert!(!recovery.resume_current);
    assert!(
        controller
            .complete(command, Err(anyhow::anyhow!("late")), &identity())
            .is_none()
    );
    assert_eq!(
        controller.view_model().error,
        Some("切换下一集失败：offline")
    );
}

#[test]
fn cancel_and_retry_reject_old_success_failure_and_pause_failure() {
    let mut controller = QueueController::new(queue(), identity());
    let old = controller
        .begin(QueueAction::Next, false, false, false)
        .unwrap();
    controller.cancel();
    let current = controller
        .begin(QueueAction::Previous, false, false, false)
        .unwrap();
    assert!(
        controller
            .complete(old.clone(), Ok(resolved()), &identity())
            .is_none()
    );
    assert!(
        controller
            .complete(old.clone(), Err(anyhow::anyhow!("late")), &identity())
            .is_none()
    );
    assert!(!controller.pause_failed(&old.token, &identity(), "late pause"));
    assert!(controller.view_model().loading);
    assert!(controller.view_model().error.is_none());
    let Some(QueueSwitchUpdate::Replace(replacement)) =
        controller.complete(current, Ok(resolved()), &identity())
    else {
        panic!("expected replacement");
    };
    assert_eq!(replacement.queue.current_index, 0);
    assert_eq!(controller.queue().current_index, 1);
    assert!(!controller.view_model().loading);
}

#[test]
fn foreign_account_and_same_account_other_owner_cannot_complete_request() {
    let mut controller = QueueController::new(queue(), identity());
    let command = controller
        .begin(QueueAction::Next, false, false, false)
        .unwrap();
    for other in [
        WorkspaceIdentity {
            local_server_id: "other".into(),
            ..identity()
        },
        WorkspaceIdentity {
            remote_server_id: None,
            ..identity()
        },
        WorkspaceIdentity {
            user_id: None,
            ..identity()
        },
    ] {
        assert!(
            controller
                .complete(command.clone(), Ok(resolved()), &other)
                .is_none()
        );
        assert!(
            controller
                .complete(command.clone(), Err(anyhow::anyhow!("foreign")), &other)
                .is_none()
        );
        assert!(!controller.pause_failed(&command.token, &other, "foreign"));
    }
    let mut foreign = QueueController::new(queue(), identity());
    let other = foreign
        .begin(QueueAction::Next, false, false, false)
        .unwrap();
    assert!(
        controller
            .complete(other, Ok(resolved()), &identity())
            .is_none()
    );
    assert!(controller.view_model().loading);
    assert!(controller.view_model().error.is_none());
    assert!(matches!(
        controller.complete(command, Ok(resolved()), &identity()),
        Some(QueueSwitchUpdate::Replace(_))
    ));
}

#[test]
fn invalid_targets_do_not_start_requests_or_clear_an_existing_error() {
    let mut controller = QueueController::new(queue(), identity());
    let command = controller
        .begin(QueueAction::Previous, false, false, false)
        .unwrap();
    assert!(controller.pause_failed(&command.token, &identity(), "pause failed"));
    let error = controller.view_model().error.unwrap().to_owned();
    for action in [
        QueueAction::Select(1),
        QueueAction::Select(3),
        QueueAction::Select(usize::MAX),
    ] {
        assert!(controller.begin(action, false, false, false).is_none());
        assert!(!controller.view_model().loading);
        assert_eq!(controller.view_model().error, Some(error.as_str()));
    }
    let mut empty = QueueController::new(crate::player::PlaybackQueue::new(vec![], 0), identity());
    for action in [
        QueueAction::Previous,
        QueueAction::Next,
        QueueAction::Select(2),
    ] {
        assert!(empty.begin(action, false, false, false).is_none());
    }
    assert!(!empty.view_model().can_previous() && !empty.view_model().can_next());
}

#[test]
fn failed_pause_is_terminal_and_retry_clears_error_with_action_specific_prefix() {
    for (action, prefix) in [
        (QueueAction::Previous, "切换上一集失败"),
        (QueueAction::Next, "切换下一集失败"),
        (QueueAction::Select(2), "切换剧集失败"),
    ] {
        let mut controller = QueueController::new(queue(), identity());
        let command = controller.begin(action, false, false, false).unwrap();
        assert!(controller.pause_failed(&command.token, &identity(), "pause"));
        assert!(!controller.pause_failed(&command.token, &identity(), "duplicate"));
        assert!(
            controller
                .complete(command, Ok(resolved()), &identity())
                .is_none()
        );
        assert_eq!(
            controller.view_model().error,
            Some(format!("{prefix}：pause").as_str())
        );
        let command = controller.begin(action, false, false, false).unwrap();
        assert!(controller.view_model().error.is_none());
        controller.complete(command, Err(anyhow::anyhow!("network")), &identity());
        controller.resume_failed("audio");
        assert_eq!(
            controller.view_model().error,
            Some(format!("{prefix}：network；恢复当前播放失败：audio").as_str())
        );
    }
}

#[test]
fn replacement_preserves_previous_position_or_restarts_completed_episode() {
    for ended in [false, true] {
        let mut controller = QueueController::new(queue(), identity());
        let command = controller
            .begin(QueueAction::Next, false, false, ended)
            .unwrap();
        let Some(QueueSwitchUpdate::Replace(mut replacement)) =
            controller.complete(command, Ok(resolved()), &identity())
        else {
            panic!("expected replacement");
        };
        let mut update = PlaybackStateUpdate {
            item_id: "old-physical".into(),
            list_item_id: "grouped-1".into(),
            media_source_id: "old-source".into(),
            media_source_name: None,
            series_id: Some("series".into()),
            season_id: Some("season".into()),
            position_ticks: 123_456_789,
            run_time_ticks: Some(900_000_000),
            ended,
            failed: false,
            selected_item_id: None,
            stop_completion: None,
        };
        replacement.apply_close(&mut update);
        assert_eq!(
            replacement.queue.items[1].playback_position_ticks,
            Some(if ended { 0 } else { 123_456_789 })
        );
        assert_eq!(
            replacement.queue.items[2].playback_position_ticks,
            Some(25_000_000)
        );
        assert_eq!(
            replacement
                .queue
                .current()
                .unwrap()
                .premiere_date
                .as_deref(),
            Some("2026-01-01")
        );
        assert_eq!(update.item_id, "old-physical");
        assert_eq!(update.list_item_id, "grouped-1");
        assert_eq!(update.selected_item_id.as_deref(), Some("grouped-2"));
        assert_eq!(replacement.playback.source.item_id, "resolved-physical");
        assert_eq!(controller.queue().current_index, 1);
    }
}
