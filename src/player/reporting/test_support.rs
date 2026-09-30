use super::controller::*;
use crate::{
    effects::WorkspaceIdentity,
    player::model::queue::{PlaybackQueue, PlaybackQueueItem},
};

pub(in crate::player) struct Session {
    pub(in crate::player) controller: ReportingController,
    pub(in crate::player) identity: WorkspaceIdentity,
    pub(in crate::player) queue: PlaybackQueue,
    pub(in crate::player) telemetry: ReportTelemetry,
    pub(in crate::player) runtime: Option<u64>,
}

impl Session {
    pub(in crate::player) fn new() -> Self {
        let identity = WorkspaceIdentity {
            local_server_id: "local".into(),
            remote_server_id: Some("remote".into()),
            user_id: Some("user".into()),
        };
        let item = |id: &str| PlaybackQueueItem {
            item_id: id.into(),
            title: id.into(),
            episode_label: "S1E1".into(),
            overview: None,
            primary_image_tag: None,
            series_id: Some("series".into()),
            season_id: Some("season".into()),
            premiere_date: None,
            run_time_ticks: Some(900_000_000),
            playback_position_ticks: None,
            media_sources: vec![
                serde_json::from_value(serde_json::json!({"Id":"source", "Name":"HD"})).unwrap(),
            ],
        };
        Self {
            controller: ReportingController::new(identity.clone()),
            identity,
            queue: PlaybackQueue::new(vec![item("previous"), item("grouped"), item("next")], 1),
            telemetry: ReportTelemetry {
                can_seek: true,
                audio: Some(2),
                subtitle: Some(4),
                user_paused: false,
                volume: 0.755,
                position: Some(2.5),
                drag_position: None,
                duration: Some(90.0),
            },
            runtime: Some(900_000_000),
        }
    }

    pub(in crate::player) fn dispatch(&mut self, intent: ReportingIntent) -> ReportingTransition {
        self.controller.dispatch(
            intent,
            ReportContext {
                item_id: "physical",
                media_source_id: "source",
                play_session_id: Some("session"),
                run_time_ticks: self.runtime,
                queue: &self.queue,
            },
            self.telemetry,
        )
    }
}
