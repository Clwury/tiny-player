use std::{sync::mpsc::Sender, time::Instant};

use crate::player::{
    backend::{BackendEvent, BackendEventKind},
    render_host::PlaybackSessionId,
};

use super::{POSITION_QUERY_INTERVAL, max_optional_seconds, optional_buffered_value_changed};

#[derive(Default)]
pub(super) struct PositionReporter {
    last_report: Option<Instant>,
    last_position: Option<f64>,
}

impl PositionReporter {
    pub(super) fn report(
        &mut self,
        pts_nsecs: u64,
        session_id: PlaybackSessionId,
        event_tx: &Sender<BackendEvent>,
    ) {
        if self
            .last_report
            .is_some_and(|last| last.elapsed() < POSITION_QUERY_INTERVAL)
        {
            return;
        }

        let position = pts_nsecs as f64 / 1_000_000_000.0;
        if self
            .last_position
            .is_some_and(|last| (last - position).abs() < 0.05)
        {
            return;
        }

        self.last_report = Some(Instant::now());
        self.last_position = Some(position);
        let _ = event_tx.send(BackendEvent::new(
            session_id,
            BackendEventKind::PositionChanged(position),
        ));
    }
}

pub(super) struct BufferedReporter {
    last_report: Option<Instant>,
    last_buffered_until: Option<f64>,
    video_buffered_until: Option<f64>,
    audio_buffered_until: Option<f64>,
    needs_audio: bool,
    emit_events: bool,
}

impl BufferedReporter {
    pub(super) fn new_with_events(needs_audio: bool, emit_events: bool) -> Self {
        Self {
            last_report: None,
            last_buffered_until: None,
            video_buffered_until: None,
            audio_buffered_until: None,
            needs_audio,
            emit_events,
        }
    }

    pub(super) fn reset_to(
        &mut self,
        position_seconds: f64,
        session_id: PlaybackSessionId,
        event_tx: &Sender<BackendEvent>,
    ) {
        let position_seconds = position_seconds.max(0.0);
        self.last_report = None;
        self.last_buffered_until = None;
        self.video_buffered_until = Some(position_seconds);
        self.audio_buffered_until = self.needs_audio.then_some(position_seconds);
        self.report_value(Some(position_seconds), session_id, event_tx);
        self.last_report = None;
    }

    pub(super) fn report_video_timeline_nsecs(
        &mut self,
        timeline_nsecs: u64,
        session_id: PlaybackSessionId,
        event_tx: &Sender<BackendEvent>,
    ) {
        self.video_buffered_until = Some(max_optional_seconds(
            self.video_buffered_until,
            timeline_nsecs,
        ));
        self.report_combined(session_id, event_tx);
    }

    pub(super) fn report_audio_timeline_nsecs(
        &mut self,
        timeline_nsecs: u64,
        session_id: PlaybackSessionId,
        event_tx: &Sender<BackendEvent>,
    ) {
        self.audio_buffered_until = Some(max_optional_seconds(
            self.audio_buffered_until,
            timeline_nsecs,
        ));
        self.report_combined(session_id, event_tx);
    }

    pub(super) fn report_combined(
        &mut self,
        session_id: PlaybackSessionId,
        event_tx: &Sender<BackendEvent>,
    ) {
        if self
            .last_report
            .is_some_and(|last| last.elapsed() < POSITION_QUERY_INTERVAL)
        {
            return;
        }

        let Some(buffered_until) = (if self.needs_audio {
            self.video_buffered_until
                .zip(self.audio_buffered_until)
                .map(|(video, audio)| video.min(audio))
        } else {
            self.video_buffered_until
        }) else {
            return;
        };
        let buffered_until = self
            .last_buffered_until
            .map(|last| last.max(buffered_until))
            .unwrap_or(buffered_until);
        self.report_value(Some(buffered_until), session_id, event_tx);
    }

    pub(super) fn buffered_until(&self) -> Option<f64> {
        if self.needs_audio {
            self.video_buffered_until
                .zip(self.audio_buffered_until)
                .map(|(video, audio)| video.min(audio))
        } else {
            self.video_buffered_until
        }
    }

    pub(super) fn report_value(
        &mut self,
        buffered_until: Option<f64>,
        session_id: PlaybackSessionId,
        event_tx: &Sender<BackendEvent>,
    ) {
        if !optional_buffered_value_changed(self.last_buffered_until, buffered_until) {
            return;
        }

        self.last_report = Some(Instant::now());
        self.last_buffered_until = buffered_until;
        if self.emit_events {
            let _ = event_tx.send(BackendEvent::new(
                session_id,
                BackendEventKind::BufferedChanged(buffered_until),
            ));
        }
    }
}
