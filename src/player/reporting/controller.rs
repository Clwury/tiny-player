use super::{PlaybackStateUpdate, PlaybackStopCompletion};
use crate::{
    effects::{RequestScope, RequestSlot, RequestToken, WorkspaceIdentity},
    emby::{PlaybackProgressReport, PlaybackStartReport, PlaybackStopReport},
    media::PlaybackQueue,
};
use tiny_playback::clamp_playback_volume;

pub(crate) enum PlaybackReport {
    Started(PlaybackStartReport),
    Progress(PlaybackProgressReport),
    Stopped(PlaybackStopReport),
}

impl PlaybackReport {
    pub(super) fn scope(&self) -> RequestScope {
        match self {
            Self::Started(_) => RequestScope::PlaybackReportStart,
            Self::Progress(_) => RequestScope::PlaybackReportProgress,
            Self::Stopped(_) => RequestScope::PlaybackReportStop,
        }
    }
}

/// Ownership transfers to the serial worker. Terminal reports must survive page
/// release. The receipt is failed if delivery/worker shuts down without a result.
pub(in crate::player) struct ReportingCommand {
    pub(super) report: PlaybackReport,
    completion: Option<PlaybackStopCompletion>,
    slot: RequestSlot,
    token: RequestToken,
}

impl ReportingCommand {
    fn new(
        report: PlaybackReport,
        completion: Option<PlaybackStopCompletion>,
        identity: WorkspaceIdentity,
    ) -> Self {
        let mut slot = RequestSlot::new(report.scope(), identity);
        let token = slot.issue();
        Self {
            report,
            completion,
            slot,
            token,
        }
    }

    #[cfg(test)]
    pub(in crate::player) fn report(&self) -> &PlaybackReport {
        &self.report
    }

    pub(super) fn accepts(&self, identity: &WorkspaceIdentity) -> bool {
        self.token.is_for(identity) && self.token.is_current(&self.slot)
    }

    pub(super) fn finish(mut self, identity: &WorkspaceIdentity, succeeded: bool) {
        if self.token.is_for(identity)
            && self.slot.commit(&self.token)
            && let Some(completion) = self.completion.take()
        {
            completion.finish(succeeded);
        }
    }
}

impl Drop for ReportingCommand {
    fn drop(&mut self) {
        if let Some(completion) = &self.completion {
            completion.finish(false);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Prepared,
    Started,
    Closed,
}

/// Pure session reporting owner. Intents produce immutable reports; timer
/// acceptance uses a session-owned slot. Close is terminal and idempotent.
pub(in crate::player) struct ReportingController {
    identity: WorkspaceIdentity,
    phase: Phase,
    periodic: RequestSlot,
    scheduled: bool,
    last_progress_snapshot: Option<PlaybackReportSnapshot>,
    closed_update: Option<PlaybackStateUpdate>,
}

pub(in crate::player) enum ReportingIntent {
    Restart,
    Progress { force: bool },
    Close { failed: bool, ended: bool },
}

#[derive(Default)]
pub(in crate::player) struct ReportingTransition {
    pub(in crate::player) started: bool,
    pub(in crate::player) command: Option<ReportingCommand>,
    pub(in crate::player) update: Option<PlaybackStateUpdate>,
}

pub(in crate::player) struct ReportContext<'a> {
    pub(in crate::player) item_id: &'a str,
    pub(in crate::player) media_source_id: &'a str,
    pub(in crate::player) play_session_id: Option<&'a str>,
    pub(in crate::player) run_time_ticks: Option<u64>,
    pub(in crate::player) queue: &'a PlaybackQueue,
}

#[derive(Clone, Copy)]
pub(in crate::player) struct ReportTelemetry {
    pub(in crate::player) can_seek: bool,
    pub(in crate::player) audio: Option<usize>,
    pub(in crate::player) subtitle: Option<usize>,
    pub(in crate::player) user_paused: bool,
    pub(in crate::player) volume: f32,
    pub(in crate::player) position: Option<f64>,
    pub(in crate::player) drag_position: Option<f64>,
    pub(in crate::player) duration: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaybackReportSnapshot {
    can_seek: bool,
    audio_stream_index: Option<i32>,
    subtitle_stream_index: Option<i32>,
    is_paused: bool,
    is_muted: bool,
    position_ticks: u64,
    run_time_ticks: Option<u64>,
    volume_level: i32,
}

impl ReportTelemetry {
    fn snapshot(self, context: &ReportContext<'_>, ended: bool) -> PlaybackReportSnapshot {
        let runtime = context.run_time_ticks.or_else(|| {
            self.duration
                .map(|duration| playback_seconds_to_ticks(duration, None))
                .filter(|ticks| *ticks > 0)
        });
        let position =
            playback_seconds_to_ticks(self.drag_position.or(self.position).unwrap_or(0.0), runtime);
        PlaybackReportSnapshot {
            can_seek: self.can_seek,
            audio_stream_index: stream_index_i32(self.audio),
            subtitle_stream_index: stream_index_i32(self.subtitle),
            is_paused: self.user_paused,
            is_muted: self.volume <= f32::EPSILON,
            position_ticks: if ended {
                runtime
                    .or_else(|| {
                        self.duration
                            .map(|duration| playback_seconds_to_ticks(duration, None))
                    })
                    .unwrap_or(position)
            } else {
                position
            },
            run_time_ticks: runtime,
            volume_level: playback_volume_level(self.volume),
        }
    }
}

impl ReportingController {
    pub(in crate::player) fn new(identity: WorkspaceIdentity) -> Self {
        Self {
            periodic: RequestSlot::new(RequestScope::PlaybackReportTimer, identity.clone()),
            identity,
            phase: Phase::Prepared,
            scheduled: false,
            last_progress_snapshot: None,
            closed_update: None,
        }
    }

    pub(in crate::player) fn is_closed(&self) -> bool {
        self.phase == Phase::Closed
    }

    pub(in crate::player) fn dispatch(
        &mut self,
        intent: ReportingIntent,
        context: ReportContext<'_>,
        telemetry: ReportTelemetry,
    ) -> ReportingTransition {
        let mut transition = ReportingTransition::default();
        let ended = matches!(intent, ReportingIntent::Close { ended: true, .. });
        let snapshot = telemetry.snapshot(&context, ended);
        let playlist_index = i32::try_from(context.queue.current_index).unwrap_or(i32::MAX);
        let report = match intent {
            ReportingIntent::Restart if self.phase == Phase::Prepared => {
                let mut report = PlaybackStartReport::direct_stream(
                    context.item_id.into(),
                    context.media_source_id.into(),
                    PlaybackQueue::playlist_item_id(context.queue.current_index),
                    context.queue.report_items(context.item_id),
                );
                apply_snapshot_to_start_report(&snapshot, &mut report);
                report.play_session_id = context.play_session_id.map(str::to_owned);
                report.playlist_index = playlist_index;
                self.phase = Phase::Started;
                self.last_progress_snapshot = Some(snapshot);
                transition.started = true;
                Some(PlaybackReport::Started(report))
            }
            ReportingIntent::Restart | ReportingIntent::Progress { .. }
                if self.phase == Phase::Started =>
            {
                let force = !matches!(intent, ReportingIntent::Progress { force: false });
                if !force && self.last_progress_snapshot.as_ref() == Some(&snapshot) {
                    return transition;
                }
                let mut report = PlaybackProgressReport::direct_stream(
                    context.item_id.into(),
                    context.media_source_id.into(),
                    PlaybackQueue::playlist_item_id(context.queue.current_index),
                    context.queue.items.len(),
                    playlist_index,
                );
                apply_snapshot_to_progress_report(&snapshot, &mut report);
                report.play_session_id = context.play_session_id.map(str::to_owned);
                self.last_progress_snapshot = Some(snapshot);
                Some(PlaybackReport::Progress(report))
            }
            ReportingIntent::Close { failed, ended } => {
                if let Some(update) = &self.closed_update {
                    transition.update = Some(update.clone());
                    return transition;
                }
                let completion =
                    (self.phase == Phase::Started).then(PlaybackStopCompletion::pending);
                let report = completion.as_ref().map(|_| {
                    let mut report = PlaybackStopReport::direct_stream(
                        context.item_id.into(),
                        context.media_source_id.into(),
                    );
                    report.can_seek = snapshot.can_seek;
                    report.is_paused = snapshot.is_paused;
                    report.failed = failed;
                    report.position_ticks = snapshot.position_ticks;
                    report.play_session_id = context.play_session_id.map(str::to_owned);
                    PlaybackReport::Stopped(report)
                });
                let item = context.queue.current();
                let update = PlaybackStateUpdate {
                    item_id: context.item_id.into(),
                    list_item_id: item
                        .map(|item| item.item_id.clone())
                        .unwrap_or_else(|| context.item_id.into()),
                    media_source_id: context.media_source_id.into(),
                    media_source_name: item
                        .and_then(|item| {
                            item.media_sources.iter().find(|source| {
                                source.id.as_deref() == Some(context.media_source_id)
                            })
                        })
                        .and_then(|source| source.name.clone()),
                    series_id: item.and_then(|item| item.series_id.clone()),
                    season_id: item.and_then(|item| item.season_id.clone()),
                    position_ticks: snapshot.position_ticks,
                    run_time_ticks: snapshot.run_time_ticks,
                    failed,
                    ended,
                    selected_item_id: None,
                    stop_completion: completion.clone(),
                };
                self.phase = Phase::Closed;
                self.periodic.invalidate();
                self.scheduled = false;
                self.closed_update = Some(update.clone());
                transition.update = Some(update);
                transition.command = report
                    .map(|report| ReportingCommand::new(report, completion, self.identity.clone()));
                return transition;
            }
            _ => None,
        };
        transition.command =
            report.map(|report| ReportingCommand::new(report, None, self.identity.clone()));
        transition
    }

    pub(in crate::player) fn begin_periodic(&mut self) -> Option<RequestToken> {
        if self.phase != Phase::Started || self.scheduled {
            return None;
        }
        self.scheduled = true;
        Some(self.periodic.issue())
    }

    pub(in crate::player) fn accept_periodic(
        &mut self,
        token: &RequestToken,
        identity: &WorkspaceIdentity,
    ) -> bool {
        if self.phase != Phase::Started || !token.is_for(identity) || !self.periodic.commit(token) {
            return false;
        }
        self.scheduled = false;
        true
    }
}

fn apply_snapshot_to_start_report(
    snapshot: &PlaybackReportSnapshot,
    report: &mut PlaybackStartReport,
) {
    report.can_seek = snapshot.can_seek;
    report.audio_stream_index = snapshot.audio_stream_index;
    report.subtitle_stream_index = snapshot.subtitle_stream_index;
    report.is_paused = snapshot.is_paused;
    report.is_muted = snapshot.is_muted;
    report.position_ticks = snapshot.position_ticks;
    report.run_time_ticks = snapshot.run_time_ticks;
    report.volume_level = snapshot.volume_level;
}

fn apply_snapshot_to_progress_report(
    snapshot: &PlaybackReportSnapshot,
    report: &mut PlaybackProgressReport,
) {
    report.can_seek = snapshot.can_seek;
    report.audio_stream_index = snapshot.audio_stream_index;
    report.subtitle_stream_index = snapshot.subtitle_stream_index;
    report.is_paused = snapshot.is_paused;
    report.is_muted = snapshot.is_muted;
    report.position_ticks = snapshot.position_ticks;
    report.run_time_ticks = snapshot.run_time_ticks;
    report.volume_level = snapshot.volume_level;
}

fn playback_seconds_to_ticks(seconds: f64, run_time_ticks: Option<u64>) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    let ticks = seconds * 10_000_000_f64;
    let ticks = if !ticks.is_finite() || ticks >= u64::MAX as f64 {
        u64::MAX
    } else {
        ticks.round() as u64
    };
    run_time_ticks
        .filter(|runtime| *runtime > 0)
        .map_or(ticks, |runtime| ticks.min(runtime))
}

fn stream_index_i32(index: Option<usize>) -> Option<i32> {
    index.and_then(|index| i32::try_from(index).ok())
}

fn playback_volume_level(volume: f32) -> i32 {
    (clamp_playback_volume(volume) * 100.0).round() as i32
}

#[cfg(test)]
mod tests {
    use super::super::PlaybackStopResult;
    use super::*;

    #[test]
    fn playback_seconds_to_ticks_rounds_and_clamps() {
        assert_eq!(playback_seconds_to_ticks(f64::NAN, None), 0);
        assert_eq!(playback_seconds_to_ticks(f64::INFINITY, None), 0);
        assert_eq!(playback_seconds_to_ticks(-1.0, None), 0);
        assert_eq!(playback_seconds_to_ticks(1.0, None), 10_000_000);
        assert_eq!(playback_seconds_to_ticks(1.25, None), 12_500_000);
        assert_eq!(playback_seconds_to_ticks(f64::MAX, None), u64::MAX);
        assert_eq!(
            playback_seconds_to_ticks(30.0, Some(20_000_000)),
            20_000_000
        );
    }

    #[test]
    fn stream_indices_reject_values_outside_emby_i32_range() {
        assert_eq!(stream_index_i32(Some(4)), Some(4));
        assert_eq!(stream_index_i32(Some(i32::MAX as usize + 1)), None);
    }

    #[test]
    fn volume_level_is_clamped_to_percentage() {
        assert_eq!(playback_volume_level(-1.0), 0);
        assert_eq!(playback_volume_level(0.755), 76);
        assert_eq!(playback_volume_level(2.0), 100);
    }

    #[test]
    fn stop_completion_records_terminal_result() {
        let completion = PlaybackStopCompletion::pending();
        assert_eq!(completion.result(), PlaybackStopResult::Pending);

        completion.finish(true);

        assert_eq!(completion.result(), PlaybackStopResult::Succeeded);
    }
}
