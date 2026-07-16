use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread,
};

use crate::emby::{PlaybackProgressReport, PlaybackStartReport, PlaybackStopReport};

use super::*;

const PLAYBACK_PROGRESS_REPORT_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct PlaybackStateUpdate {
    pub item_id: String,
    pub series_id: Option<String>,
    pub season_id: Option<String>,
    pub position_ticks: u64,
    pub run_time_ticks: Option<u64>,
    pub ended: bool,
    pub failed: bool,
    pub selected_item_id: Option<String>,
    pub stop_completion: Option<PlaybackStopCompletion>,
}

#[derive(Clone)]
pub struct PlaybackStopCompletion {
    state: Arc<AtomicU8>,
}

impl fmt::Debug for PlaybackStopCompletion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("PlaybackStopCompletion")
            .field(&self.result())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackStopResult {
    Pending,
    Succeeded,
    Failed,
}

impl PlaybackStopCompletion {
    fn pending() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(0)),
        }
    }

    pub fn result(&self) -> PlaybackStopResult {
        match self.state.load(Ordering::Acquire) {
            1 => PlaybackStopResult::Succeeded,
            2 => PlaybackStopResult::Failed,
            _ => PlaybackStopResult::Pending,
        }
    }

    fn finish(&self, succeeded: bool) {
        self.state
            .store(if succeeded { 1 } else { 2 }, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PlaybackReportingPhase {
    #[default]
    Prepared,
    Started,
    Closed,
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

pub(super) struct PlaybackReportingState {
    reporter: PlaybackReporter,
    phase: PlaybackReportingPhase,
    periodic_generation: u64,
    periodic_scheduled: bool,
    last_progress_snapshot: Option<PlaybackReportSnapshot>,
    closed_update: Option<PlaybackStateUpdate>,
}

impl PlaybackReportingState {
    pub(super) fn new(context: &EmbyPlaybackContext) -> Self {
        Self {
            reporter: PlaybackReporter::new(context.client.clone(), context.server.clone()),
            phase: PlaybackReportingPhase::Prepared,
            periodic_generation: 0,
            periodic_scheduled: false,
            last_progress_snapshot: None,
            closed_update: None,
        }
    }
}

enum PlaybackReportCommand {
    Started(PlaybackStartReport),
    Progress(PlaybackProgressReport),
    Stopped {
        report: PlaybackStopReport,
        completion: PlaybackStopCompletion,
    },
}

struct PlaybackReporter {
    tx: Option<Sender<PlaybackReportCommand>>,
}

impl PlaybackReporter {
    fn new(client: crate::emby::EmbyClient, server: crate::server::CachedServer) -> Self {
        let (tx, rx) = mpsc::channel();
        let spawn = thread::Builder::new()
            .name("tiny-emby-playback-reporter".to_string())
            .spawn(move || run_playback_reporter(client, server, rx));
        if let Err(error) = spawn {
            tracing::warn!(%error, "failed to spawn Emby playback reporter");
            return Self { tx: None };
        }
        Self { tx: Some(tx) }
    }

    fn send(&self, command: PlaybackReportCommand) -> bool {
        let Some(tx) = self.tx.as_ref() else {
            return false;
        };
        if tx.send(command).is_err() {
            tracing::warn!("Emby playback reporter is no longer available");
            return false;
        }
        true
    }
}

fn run_playback_reporter(
    client: crate::emby::EmbyClient,
    server: crate::server::CachedServer,
    rx: Receiver<PlaybackReportCommand>,
) {
    while let Ok(command) = rx.recv() {
        match command {
            PlaybackReportCommand::Started(report) => {
                let item_id = report.item_id.clone();
                if let Err(error) = client.report_playback_started(&server, &report) {
                    tracing::warn!(
                        endpoint = %server.endpoint.display_url(),
                        %item_id,
                        %error,
                        "Emby playback start report failed"
                    );
                }
            }
            PlaybackReportCommand::Progress(report) => {
                let (report, stopped) = latest_progress_or_stop(report, &rx);
                if let Some((stopped, completion)) = stopped {
                    send_stopped_report(&client, &server, stopped, &completion);
                    break;
                }
                let item_id = report.item_id.clone();
                if let Err(error) = client.report_playback_progress(&server, &report) {
                    tracing::warn!(
                        endpoint = %server.endpoint.display_url(),
                        %item_id,
                        %error,
                        "Emby playback progress report failed"
                    );
                }
            }
            PlaybackReportCommand::Stopped { report, completion } => {
                send_stopped_report(&client, &server, report, &completion);
                break;
            }
        }
    }
}

fn latest_progress_or_stop(
    mut latest: PlaybackProgressReport,
    rx: &Receiver<PlaybackReportCommand>,
) -> (
    PlaybackProgressReport,
    Option<(PlaybackStopReport, PlaybackStopCompletion)>,
) {
    loop {
        match rx.try_recv() {
            Ok(PlaybackReportCommand::Progress(report)) => latest = report,
            Ok(PlaybackReportCommand::Stopped { report, completion }) => {
                return (latest, Some((report, completion)));
            }
            Ok(PlaybackReportCommand::Started(report)) => {
                tracing::warn!(
                    item_id = %report.item_id,
                    "ignoring duplicate Emby playback start report"
                );
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return (latest, None),
        }
    }
}

fn send_stopped_report(
    client: &crate::emby::EmbyClient,
    server: &crate::server::CachedServer,
    report: PlaybackStopReport,
    completion: &PlaybackStopCompletion,
) {
    let item_id = report.item_id.clone();
    match client.report_playback_stopped(server, &report) {
        Ok(()) => completion.finish(true),
        Err(error) => {
            completion.finish(false);
            tracing::warn!(
                endpoint = %server.endpoint.display_url(),
                %item_id,
                %error,
                "Emby playback stop report failed"
            );
        }
    }
}

impl PlaybackPage {
    pub(super) fn handle_playback_restart_reporting(&mut self, cx: &mut Context<Self>) {
        match self.reporting.phase {
            PlaybackReportingPhase::Prepared => self.start_playback_reporting(cx),
            PlaybackReportingPhase::Started => self.report_playback_progress(true),
            PlaybackReportingPhase::Closed => {}
        }
    }

    pub(super) fn start_playback_reporting(&mut self, cx: &mut Context<Self>) {
        if self.reporting.phase != PlaybackReportingPhase::Prepared {
            return;
        }

        let snapshot = self.playback_report_snapshot();
        let playlist_index = self.playlist_index_i32();
        let playlist_item_id = PlaybackQueue::playlist_item_id(self.queue.current_index);
        let mut report = PlaybackStartReport::direct_stream(
            self.emby.item_id.clone(),
            self.emby.media_source_id.clone(),
            playlist_item_id,
            self.queue.report_items(),
        );
        apply_snapshot_to_start_report(&snapshot, &mut report);
        report.play_session_id = self.emby.play_session_id.clone();
        report.playlist_index = playlist_index;

        self.reporting.phase = PlaybackReportingPhase::Started;
        self.reporting.last_progress_snapshot = Some(snapshot);
        let _ = self
            .reporting
            .reporter
            .send(PlaybackReportCommand::Started(report));
        self.schedule_periodic_playback_progress(cx);
    }

    pub(super) fn report_playback_progress(&mut self, force: bool) {
        if self.reporting.phase != PlaybackReportingPhase::Started {
            return;
        }

        let snapshot = self.playback_report_snapshot();
        if !force
            && self
                .reporting
                .last_progress_snapshot
                .as_ref()
                .is_some_and(|last| last == &snapshot)
        {
            return;
        }

        let mut report = PlaybackProgressReport::direct_stream(
            self.emby.item_id.clone(),
            self.emby.media_source_id.clone(),
            PlaybackQueue::playlist_item_id(self.queue.current_index),
            self.queue.items.len(),
            self.playlist_index_i32(),
        );
        apply_snapshot_to_progress_report(&snapshot, &mut report);
        report.play_session_id = self.emby.play_session_id.clone();
        self.reporting.last_progress_snapshot = Some(snapshot);
        let _ = self
            .reporting
            .reporter
            .send(PlaybackReportCommand::Progress(report));
    }

    pub(super) fn close_playback_reporting(
        &mut self,
        failed: bool,
        ended: bool,
    ) -> PlaybackStateUpdate {
        if let Some(update) = self.reporting.closed_update.as_ref() {
            return update.clone();
        }
        let snapshot = self.playback_report_snapshot_for_close(ended);
        let mut stop_completion = None;
        if self.reporting.phase == PlaybackReportingPhase::Started {
            let mut report = PlaybackStopReport::direct_stream(
                self.emby.item_id.clone(),
                self.emby.media_source_id.clone(),
            );
            report.can_seek = snapshot.can_seek;
            report.is_paused = snapshot.is_paused;
            report.failed = failed;
            report.position_ticks = snapshot.position_ticks;
            report.play_session_id = self.emby.play_session_id.clone();
            let completion = PlaybackStopCompletion::pending();
            if !self
                .reporting
                .reporter
                .send(PlaybackReportCommand::Stopped {
                    report,
                    completion: completion.clone(),
                })
            {
                completion.finish(false);
            }
            stop_completion = Some(completion);
        }
        let update = self.playback_state_update(&snapshot, failed, ended, stop_completion);
        self.reporting.phase = PlaybackReportingPhase::Closed;
        self.reporting.periodic_generation = self.reporting.periodic_generation.wrapping_add(1);
        self.reporting.periodic_scheduled = false;
        self.reporting.closed_update = Some(update.clone());
        update
    }

    pub(super) fn playback_reporting_closed(&self) -> bool {
        self.reporting.phase == PlaybackReportingPhase::Closed
    }

    fn schedule_periodic_playback_progress(&mut self, cx: &mut Context<Self>) {
        if self.reporting.phase != PlaybackReportingPhase::Started
            || self.reporting.periodic_scheduled
        {
            return;
        }

        self.reporting.periodic_scheduled = true;
        self.reporting.periodic_generation = self.reporting.periodic_generation.wrapping_add(1);
        let generation = self.reporting.periodic_generation;
        cx.spawn(async move |page, cx| {
            Timer::after(PLAYBACK_PROGRESS_REPORT_INTERVAL).await;
            page.update(cx, |page, cx| {
                if page.reporting.periodic_generation != generation
                    || page.reporting.phase != PlaybackReportingPhase::Started
                {
                    return;
                }
                page.reporting.periodic_scheduled = false;
                page.report_playback_progress(false);
                page.schedule_periodic_playback_progress(cx);
            })
            .ok();
        })
        .detach();
    }

    fn playback_report_snapshot(&self) -> PlaybackReportSnapshot {
        PlaybackReportSnapshot {
            can_seek: self.can_seek_playback(),
            audio_stream_index: stream_index_i32(self.tracks.selected_audio_stream_index),
            subtitle_stream_index: stream_index_i32(self.tracks.selected_subtitle_stream_index),
            is_paused: self.timeline.user_paused,
            is_muted: self.volume.level <= f32::EPSILON,
            position_ticks: playback_seconds_to_ticks(
                self.timeline
                    .progress_drag_position
                    .or(self.timeline.position)
                    .unwrap_or(0.0),
                self.playback_run_time_ticks(),
            ),
            run_time_ticks: self.playback_run_time_ticks(),
            volume_level: playback_volume_level(self.volume.level),
        }
    }

    fn playback_report_snapshot_for_close(&self, ended: bool) -> PlaybackReportSnapshot {
        let mut snapshot = self.playback_report_snapshot();
        if ended {
            snapshot.position_ticks = snapshot
                .run_time_ticks
                .or_else(|| {
                    self.timeline
                        .duration
                        .map(|duration| playback_seconds_to_ticks(duration, None))
                })
                .unwrap_or(snapshot.position_ticks);
        }
        snapshot
    }

    fn playback_run_time_ticks(&self) -> Option<u64> {
        self.emby.run_time_ticks.or_else(|| {
            self.timeline
                .duration
                .map(|duration| playback_seconds_to_ticks(duration, None))
                .filter(|ticks| *ticks > 0)
        })
    }

    fn playback_state_update(
        &self,
        snapshot: &PlaybackReportSnapshot,
        failed: bool,
        ended: bool,
        stop_completion: Option<PlaybackStopCompletion>,
    ) -> PlaybackStateUpdate {
        let queue_item = self.queue.current();
        PlaybackStateUpdate {
            item_id: self.emby.item_id.clone(),
            series_id: queue_item.and_then(|item| item.series_id.clone()),
            season_id: queue_item.and_then(|item| item.season_id.clone()),
            position_ticks: snapshot.position_ticks,
            run_time_ticks: snapshot.run_time_ticks,
            ended,
            failed,
            selected_item_id: None,
            stop_completion,
        }
    }

    fn playlist_index_i32(&self) -> i32 {
        i32::try_from(self.queue.current_index).unwrap_or(i32::MAX)
    }
}

impl Drop for PlaybackPage {
    fn drop(&mut self) {
        if !self.playback_reporting_closed() {
            let ended = self.timeline.ended;
            let _ = self.close_playback_reporting(false, ended);
        }
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

pub(super) fn playback_seconds_to_ticks(seconds: f64, run_time_ticks: Option<u64>) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }
    let ticks = seconds * request::EMBY_TICKS_PER_SECOND as f64;
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

    #[test]
    fn pending_progress_is_coalesced_before_terminal_stop() {
        let (tx, rx) = mpsc::channel();
        let mut newer = progress_report(20);
        newer.position_ticks = 20;
        tx.send(PlaybackReportCommand::Progress(newer)).unwrap();
        let completion = PlaybackStopCompletion::pending();
        tx.send(PlaybackReportCommand::Stopped {
            report: PlaybackStopReport::direct_stream(
                "episode-1".to_string(),
                "source-1".to_string(),
            ),
            completion: completion.clone(),
        })
        .unwrap();

        let (latest, stopped) = latest_progress_or_stop(progress_report(10), &rx);

        assert_eq!(latest.position_ticks, 20);
        let (_, stopped_completion) = stopped.expect("terminal stop is retained");
        assert_eq!(stopped_completion.result(), PlaybackStopResult::Pending);
    }

    fn progress_report(position_ticks: u64) -> PlaybackProgressReport {
        let mut report = PlaybackProgressReport::direct_stream(
            "episode-1".to_string(),
            "source-1".to_string(),
            "playlistItem0".to_string(),
            1,
            0,
        );
        report.position_ticks = position_ticks;
        report
    }
}
