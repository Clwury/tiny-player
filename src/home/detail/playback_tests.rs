use super::{
    controller::{DetailController, DetailIntent},
    effect::{run_playback, selected_playback},
    playback::{DetailPlaybackCommand, DetailPlaybackUpdate},
};
use crate::{
    effects::WorkspaceIdentity,
    emby::{MediaSource, UserItem},
    home::model::detail::SeriesDetailModel,
    player::{
        PlaybackTrack, SavedTrackChoice,
        gateway::{PlaybackGateway, ResolvedPlayback},
    },
};
use serde_json::json;
use std::sync::Mutex;

#[derive(Default)]
struct FakePlayback {
    resolve_calls: Mutex<Vec<(String, String)>>,
    track_calls: Mutex<Vec<(String, String)>>,
    fail: bool,
}
impl PlaybackGateway for FakePlayback {
    fn report(&self, _: &crate::player::gateway::PlaybackReport) -> anyhow::Result<()> {
        panic!("detail launch must not start session reporting");
    }

    fn resolve_source(&self, item: &str, source: &str) -> anyhow::Result<ResolvedPlayback> {
        self.resolve_calls
            .lock()
            .unwrap()
            .push((item.into(), source.into()));
        if self.fail {
            anyhow::bail!("offline");
        }
        Ok(resolved())
    }
    fn subtitle_tracks(&self, _: &MediaSource, item: &str, source: &str) -> Vec<PlaybackTrack> {
        self.track_calls
            .lock()
            .unwrap()
            .push((item.into(), source.into()));
        Vec::new()
    }
}
fn movie() -> DetailController {
    let item: UserItem =
        serde_json::from_value(json!({"Id":"list", "Name":"Movie", "Type":"Movie"})).unwrap();
    let mut model = SeriesDetailModel::new_movie(&item);
    model.item = Some(serde_json::from_value(json!({
        "Id":"list", "Name":"Movie", "Type":"Movie", "RunTimeTicks":100_000_000,
        "UserData":{"PlaybackPositionTicks":12_345_678},
        "MediaSources":[{"Id":"source", "ItemId":"physical", "Type":"Grouping"}, {"Id":"other", "ItemId":"other-physical"}]
    })).unwrap());
    model.sync_media_source_selection();
    DetailController::new(model, WorkspaceIdentity::default())
}
fn resolved() -> ResolvedPlayback {
    ResolvedPlayback {
        item_id: "resolved".into(),
        media_source_id: "resolved-source".into(),
        url: "https://example.invalid/video".into(),
        http_headers: vec![("X-Test".into(), "value".into())],
        content_length: Some(123),
        play_session_id: Some("session".into()),
    }
}
fn begin(controller: &mut DetailController, gateway: &FakePlayback) -> DetailPlaybackCommand {
    let selection = selected_playback(
        controller.view_model(),
        gateway,
        Default::default(),
        &Default::default(),
    );
    controller
        .begin_playback(selection, WorkspaceIdentity::default())
        .unwrap()
        .unwrap()
}

#[test]
fn prepared_playback_keeps_grouped_ids_position_and_draft_until_success() {
    let mut controller = movie();
    controller.dispatch(DetailIntent::Subtitle(None)).unwrap();
    assert_eq!(
        controller.view_model().pending_subtitle_choice(),
        Some(&SavedTrackChoice::Off)
    );
    let gateway = FakePlayback {
        fail: true,
        ..Default::default()
    };
    let command = begin(&mut controller, &gateway);
    assert_eq!(command.selected.item_id, "physical");
    assert_eq!(command.selected.list_item_id, "list");
    assert_eq!(command.selected.queue.current().unwrap().item_id, "list");
    assert_eq!(command.selected.playback_position_ticks, Some(12_345_678));
    assert!(
        controller
            .begin_playback(Ok(command.selected.clone()), WorkspaceIdentity::default())
            .unwrap()
            .is_none()
    );
    let response = run_playback(&gateway, &command);
    assert!(
        matches!(controller.complete_playback(command.clone(), response, &WorkspaceIdentity::default()), Some(DetailPlaybackUpdate::Failed(message)) if message == "获取播放地址失败：offline")
    );
    assert!(controller.view_model().pending_subtitle_choice().is_some());
    let gateway = FakePlayback::default();
    let retry = begin(&mut controller, &gateway);
    assert!(
        controller
            .complete_playback(command, Ok(resolved()), &WorkspaceIdentity::default())
            .is_none()
    );
    let response = run_playback(&gateway, &retry);
    let Some(DetailPlaybackUpdate::Open { selected, playback }) =
        controller.complete_playback(retry.clone(), response, &WorkspaceIdentity::default())
    else {
        panic!("expected playback");
    };
    assert_eq!(selected.title.as_ref(), "Movie");
    assert_eq!(playback.item_id, "resolved");
    assert_eq!(playback.http_headers, [("X-Test".into(), "value".into())]);
    assert_eq!(playback.content_length, Some(123));
    assert_eq!(playback.play_session_id.as_deref(), Some("session"));
    assert!(controller.view_model().pending_subtitle_choice().is_none());
    assert_eq!(
        *gateway.resolve_calls.lock().unwrap(),
        [("physical".into(), "source".into())]
    );
    assert_eq!(
        *gateway.track_calls.lock().unwrap(),
        [("physical".into(), "source".into())]
    );
    assert!(
        controller
            .complete_playback(
                retry,
                Err(anyhow::anyhow!("duplicate")),
                &WorkspaceIdentity::default()
            )
            .is_none()
    );
    assert!(controller.view_model().playback_failed.is_none());
}

#[test]
fn source_round_trip_navigation_and_workspace_changes_reject_old_playback_results() {
    let identity = WorkspaceIdentity::default();
    let foreign = WorkspaceIdentity {
        user_id: Some("other".into()),
        ..Default::default()
    };
    let mut controller = movie();
    let gateway = FakePlayback::default();
    let old = begin(&mut controller, &gateway);
    assert!(
        controller
            .complete_playback(old.clone(), Ok(resolved()), &foreign)
            .is_none()
    );
    assert!(controller.view_model().playback_loading);
    let update = controller.dispatch(DetailIntent::MediaSource(1)).unwrap();
    assert!(update.playback_cancelled);
    controller.dispatch(DetailIntent::MediaSource(0)).unwrap();
    let current = begin(&mut controller, &gateway);
    assert!(
        controller
            .complete_playback(old, Ok(resolved()), &identity)
            .is_none()
    );
    assert!(controller.view_model().playback_loading);
    let activation = controller.activation().cloned();
    controller.deactivate();
    assert!(controller.activation().is_none());
    assert!(
        controller
            .complete_playback(current.clone(), Err(anyhow::anyhow!("hidden")), &identity)
            .is_none()
    );
    controller.activate();
    assert_ne!(controller.activation(), activation.as_ref());
    let restored = begin(&mut controller, &gateway);
    assert!(
        controller
            .complete_playback(current, Ok(resolved()), &identity)
            .is_none()
    );
    assert!(matches!(
        controller.complete_playback(restored, Ok(resolved()), &identity),
        Some(DetailPlaybackUpdate::Open { .. })
    ));
}

#[test]
fn missing_selection_reports_an_error_without_starting_io_or_consuming_a_draft() {
    let mut controller = movie();
    controller.state.item = None;
    let gateway = FakePlayback::default();
    let selection = selected_playback(
        controller.view_model(),
        &gateway,
        Default::default(),
        &Default::default(),
    );
    assert!(
        matches!(controller.begin_playback(selection, WorkspaceIdentity::default()), Err(message) if message == "请选择要播放的媒体")
    );
    assert!(!controller.view_model().playback_loading);
    assert!(gateway.resolve_calls.lock().unwrap().is_empty());
    assert!(gateway.track_calls.lock().unwrap().is_empty());
}
