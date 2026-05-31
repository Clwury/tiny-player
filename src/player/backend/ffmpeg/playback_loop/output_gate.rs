use super::audio_output_gate::{flush_pending_start_audio, push_decoded_audio_to_output};
use super::output_rebuffer::{
    AudioClockResumeDecision, PlaybackOutputState, PlaybackResumeWaterline, RebufferResumeAnchor,
    audio_output_buffered_until_for_resume, clear_video_output_rebuffer,
    enter_video_output_rebuffer, finish_video_output_rebuffer_if_ready,
    should_block_for_demux_read, video_output_rebuffer_should_enter,
};
use super::pending_audio_queue::PendingStartAudio;
use super::scheduled_video_queue::ScheduledVideoQueue;
use super::video_output_gate::present_first_queued_video_frame;
use super::*;

pub(in crate::player::backend::ffmpeg) struct PlaybackOutputScheduler {
    pub(in crate::player::backend::ffmpeg::playback_loop) scheduled_video_queue:
        ScheduledVideoQueue,
    pub(in crate::player::backend::ffmpeg::playback_loop) pending_start_audio: PendingStartAudio,
    pub(in crate::player::backend::ffmpeg::playback_loop) playback_output_state:
        PlaybackOutputState,
    pub(in crate::player::backend::ffmpeg::playback_loop) first_video_frame_pending: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop) video_output_underrun_started_at:
        Option<Instant>,
    pub(in crate::player::backend::ffmpeg::playback_loop) video_output_rebuffer_anchor:
        Option<RebufferResumeAnchor>,
}

impl PlaybackOutputScheduler {
    pub(in crate::player::backend::ffmpeg) fn new() -> Self {
        let playback_output_state = PlaybackOutputState::Syncing;
        Self {
            scheduled_video_queue: ScheduledVideoQueue::default(),
            pending_start_audio: PendingStartAudio::default(),
            first_video_frame_pending: playback_output_state.first_video_frame_pending(),
            playback_output_state,
            video_output_underrun_started_at: None,
            video_output_rebuffer_anchor: None,
        }
    }

    pub(in crate::player::backend::ffmpeg) fn reset(&mut self, control: &FfmpegControl) {
        self.scheduled_video_queue.clear();
        self.pending_start_audio.clear();
        clear_video_output_rebuffer(&mut self.playback_output_state, control);
        self.set_state(PlaybackOutputState::Syncing);
        self.video_output_underrun_started_at = None;
        self.video_output_rebuffer_anchor = None;
    }

    pub(in crate::player::backend::ffmpeg) fn clear_rebuffer(&mut self, control: &FfmpegControl) {
        clear_video_output_rebuffer(&mut self.playback_output_state, control);
        self.video_output_underrun_started_at = None;
        self.video_output_rebuffer_anchor = None;
        self.sync_first_video_frame_pending();
    }

    pub(in crate::player::backend::ffmpeg) fn rebuffering(&self) -> bool {
        self.playback_output_state.rebuffering()
    }

    pub(in crate::player::backend::ffmpeg) fn set_state(&mut self, state: PlaybackOutputState) {
        self.playback_output_state = state;
        self.sync_first_video_frame_pending();
    }

    pub(in crate::player::backend::ffmpeg) fn finish_rebuffer_if_ready(
        &mut self,
        waterline: PlaybackResumeWaterline,
        session_id: PlaybackSessionId,
    ) -> bool {
        if !finish_video_output_rebuffer_if_ready(
            &mut self.playback_output_state,
            waterline,
            session_id,
        ) {
            return false;
        }
        self.video_output_underrun_started_at = None;
        self.video_output_rebuffer_anchor = None;
        self.sync_first_video_frame_pending();
        true
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::player::backend::ffmpeg) fn maybe_enter_video_output_rebuffer(
        &mut self,
        now: Instant,
        video_output_underflowing: bool,
        demux_cache_insufficient: bool,
        render_backlogged: bool,
        has_audio_output: bool,
        control: &FfmpegControl,
        audio_output: Option<&AudioOutput>,
        session_id: PlaybackSessionId,
        decoded_video_forward_nsecs: Option<u64>,
    ) -> bool {
        if !video_output_rebuffer_should_enter(
            &mut self.video_output_underrun_started_at,
            now,
            video_output_underflowing,
            demux_cache_insufficient,
            render_backlogged,
            has_audio_output,
            self.playback_output_state,
        ) {
            return false;
        }
        let underrun_elapsed = self
            .video_output_underrun_started_at
            .map(|started_at| now.saturating_duration_since(started_at))
            .unwrap_or_default();
        self.video_output_rebuffer_anchor = enter_video_output_rebuffer(
            &mut self.playback_output_state,
            control,
            audio_output,
            &self.scheduled_video_queue,
            session_id,
            underrun_elapsed,
            decoded_video_forward_nsecs,
        );
        self.sync_first_video_frame_pending();
        true
    }

    fn sync_first_video_frame_pending(&mut self) {
        self.first_video_frame_pending = self.playback_output_state.first_video_frame_pending();
    }

    pub(in crate::player::backend::ffmpeg) fn snapshot(&self) -> PlaybackOutputSnapshot {
        self.snapshot_for_played_until(None)
    }

    pub(in crate::player::backend::ffmpeg) fn snapshot_for_played_until(
        &self,
        played_until_nsecs: Option<u64>,
    ) -> PlaybackOutputSnapshot {
        let queued_video_duration_nsecs = self.scheduled_video_queue.duration_nsecs();
        let queued_video_range_nsecs = self.scheduled_video_queue.range_nsecs();
        let can_measure_forward = !self.playback_output_state.first_video_frame_pending()
            && !self.playback_output_state.rebuffering();
        let queued_video_forward_nsecs = played_until_nsecs
            .filter(|_| can_measure_forward)
            .and_then(|played_until| self.scheduled_video_queue.forward_nsecs_from(played_until));
        let video_output_low_water = played_until_nsecs.is_some_and(|played_until| {
            can_measure_forward && self.scheduled_video_queue.low_water(played_until)
        });

        PlaybackOutputSnapshot {
            state: self.playback_output_state,
            first_video_frame_pending: self.first_video_frame_pending,
            rebuffering: self.playback_output_state.rebuffering(),
            queued_video_frames: self.scheduled_video_queue.len(),
            queued_video_duration_nsecs,
            queued_video_range_nsecs,
            queued_video_forward_nsecs,
            video_output_low_water,
            pending_start_audio_frames: self.pending_start_audio.len(),
            pending_start_audio_nsecs: duration_nsecs(self.pending_start_audio.buffered_duration()),
            video_output_rebuffer_anchor: self.video_output_rebuffer_anchor,
        }
    }

    pub(in crate::player::backend::ffmpeg) fn scheduled_video_queue_limit_reached(
        &self,
        needs_subtitle_prefetch: bool,
    ) -> bool {
        self.scheduled_video_queue
            .limit_reached(needs_subtitle_prefetch)
    }

    pub(in crate::player::backend::ffmpeg) fn video_decode_skip_nonref_for_pressure(
        &self,
        played_until_nsecs: Option<u64>,
        has_audio_output: bool,
    ) -> bool {
        self.scheduled_video_queue.skip_nonref_for_pressure(
            self.playback_output_state,
            played_until_nsecs,
            has_audio_output,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::player::backend::ffmpeg::playback_loop) fn push_decoded_audio_or_buffer(
        &mut self,
        output: &AudioOutput,
        control: &FfmpegControl,
        audio: DecodedAudio,
        start_timeline_nsecs: u64,
        end_timeline_nsecs: u64,
        session_id: PlaybackSessionId,
        vo_queue: &VideoOutputQueue,
        frame_presented: &AtomicBool,
        position_reporter: &mut PositionReporter,
        event_tx: &Sender<BackendEvent>,
        subtitle_pipeline: &mut SubtitlePipeline,
        buffered_reporter: &mut BufferedReporter,
    ) -> std::result::Result<(), String> {
        if self.decoded_audio_can_push_directly(end_timeline_nsecs) {
            push_decoded_audio_to_output(
                output,
                control,
                audio,
                start_timeline_nsecs,
                end_timeline_nsecs,
                &mut self.pending_start_audio,
                &mut self.scheduled_video_queue,
                session_id,
                vo_queue,
                frame_presented,
                position_reporter,
                event_tx,
                subtitle_pipeline,
                buffered_reporter,
            )?;
        } else {
            self.pending_start_audio
                .push(audio, start_timeline_nsecs, end_timeline_nsecs);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::player::backend::ffmpeg::playback_loop) fn flush_pending_start_audio_if_ready(
        &mut self,
        output: &AudioOutput,
        control: &FfmpegControl,
        session_id: PlaybackSessionId,
        vo_queue: &VideoOutputQueue,
        frame_presented: &AtomicBool,
        position_reporter: &mut PositionReporter,
        event_tx: &Sender<BackendEvent>,
        subtitle_pipeline: &mut SubtitlePipeline,
        buffered_reporter: &mut BufferedReporter,
    ) -> std::result::Result<(), String> {
        if self.playback_output_state.first_video_frame_pending()
            || self.playback_output_state.rebuffering()
            || self.pending_start_audio.is_empty()
        {
            return Ok(());
        }
        let audio_snapshot = output.snapshot()?;
        let audio_start_timeline_nsecs = audio_snapshot.played_timeline_nsecs;
        let audio_flush_until_timeline_nsecs = self
            .scheduled_video_queue
            .audio_output_lead_until_nsecs()
            .unwrap_or(audio_start_timeline_nsecs);
        flush_pending_start_audio(
            &mut self.pending_start_audio,
            output,
            audio_start_timeline_nsecs,
            audio_flush_until_timeline_nsecs,
            control,
            &mut self.scheduled_video_queue,
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            subtitle_pipeline,
            buffered_reporter,
        )
        .map(|_| ())
    }

    fn decoded_audio_can_push_directly(&self, end_timeline_nsecs: u64) -> bool {
        !self.playback_output_state.first_video_frame_pending()
            && !self.playback_output_state.rebuffering()
            && self.pending_start_audio.is_empty()
            && self
                .scheduled_video_queue
                .audio_output_lead_until_nsecs()
                .is_some_and(|limit| end_timeline_nsecs <= limit)
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn push_decoded_video_for_test(
        &mut self,
        frame: QueuedVideoFrame,
    ) {
        self.scheduled_video_queue.push_queued(frame);
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn push_pending_start_audio_for_test(
        &mut self,
        audio: DecodedAudio,
        start_timeline_nsecs: u64,
        end_timeline_nsecs: u64,
    ) {
        self.pending_start_audio
            .push(audio, start_timeline_nsecs, end_timeline_nsecs);
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn set_video_output_underrun_started_at_for_test(
        &mut self,
        started_at: Instant,
    ) {
        self.video_output_underrun_started_at = Some(started_at);
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn video_output_underrun_started_for_test(
        &self,
    ) -> bool {
        self.video_output_underrun_started_at.is_some()
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn set_video_output_rebuffer_anchor_for_test(
        &mut self,
        anchor: RebufferResumeAnchor,
    ) {
        self.video_output_rebuffer_anchor = Some(anchor);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct PlaybackOutputSnapshot {
    pub(in crate::player::backend::ffmpeg) state: PlaybackOutputState,
    pub(in crate::player::backend::ffmpeg) first_video_frame_pending: bool,
    pub(in crate::player::backend::ffmpeg) rebuffering: bool,
    pub(in crate::player::backend::ffmpeg) queued_video_frames: usize,
    pub(in crate::player::backend::ffmpeg) queued_video_duration_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) queued_video_range_nsecs: Option<(u64, u64)>,
    pub(in crate::player::backend::ffmpeg) queued_video_forward_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) video_output_low_water: bool,
    pub(in crate::player::backend::ffmpeg) pending_start_audio_frames: usize,
    pub(in crate::player::backend::ffmpeg) pending_start_audio_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) video_output_rebuffer_anchor:
        Option<RebufferResumeAnchor>,
}

impl PlaybackOutputSnapshot {
    pub(in crate::player::backend::ffmpeg) fn waiting_for_demux(self) -> bool {
        !self.first_video_frame_pending && self.queued_video_frames == 0
    }

    pub(in crate::player::backend::ffmpeg) fn underflowing(self) -> bool {
        self.waiting_for_demux() || self.video_output_low_water
    }

    pub(in crate::player::backend::ffmpeg) fn should_wait_for_demux(self) -> bool {
        should_block_for_demux_read(self.state)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum OutputGateResumeStatus {
    Idle,
    Waiting,
    WaitingForDemux,
    Resumed,
}

#[allow(clippy::too_many_arguments)]
pub(in crate::player::backend::ffmpeg) fn service_output_gate_resume_if_ready<F>(
    output_scheduler: &mut PlaybackOutputScheduler,
    output: Option<&AudioOutput>,
    control: &FfmpegControl,
    session_id: PlaybackSessionId,
    vo_queue: &VideoOutputQueue,
    frame_presented: &AtomicBool,
    position_reporter: &mut PositionReporter,
    event_tx: &Sender<BackendEvent>,
    subtitle_pipeline: &mut SubtitlePipeline,
    buffered_reporter: &mut BufferedReporter,
    fallback_timeline_nsecs: u64,
    current_start_position_nsecs: &mut u64,
    scheduler: &mut PlaybackScheduler,
    mut demux_watermark: F,
) -> std::result::Result<OutputGateResumeStatus, String>
where
    F: FnMut() -> DemuxReaderWatermark,
{
    let Some(output) = output else {
        return Ok(OutputGateResumeStatus::Idle);
    };
    if !output_scheduler
        .playback_output_state
        .first_video_frame_pending()
        && !output_scheduler.playback_output_state.rebuffering()
    {
        return Ok(OutputGateResumeStatus::Idle);
    }
    if output_scheduler.scheduled_video_queue.is_empty() {
        return Ok(OutputGateResumeStatus::Idle);
    }

    let needs_prefetch = subtitle_pipeline.needs_prefetch();
    let audio_snapshot = output.snapshot()?;
    let previous_audio_played_until = audio_snapshot.played_timeline_nsecs;
    let rebuffer_anchor = output_scheduler
        .playback_output_state
        .rebuffering()
        .then_some(output_scheduler.video_output_rebuffer_anchor)
        .flatten();
    let resume_audio_played_until = rebuffer_anchor
        .map(|anchor| anchor.timeline_nsecs)
        .unwrap_or(previous_audio_played_until);
    let audio_output_buffered_until_nsecs = if output_scheduler.playback_output_state.rebuffering()
    {
        Some(audio_snapshot.buffered_until_timeline_nsecs)
    } else {
        None
    };
    let resume_decision = if output_scheduler
        .playback_output_state
        .first_video_frame_pending()
    {
        output_scheduler
            .scheduled_video_queue
            .initial_audio_clock_resume_decision(
                &output_scheduler.pending_start_audio,
                previous_audio_played_until,
            )
    } else {
        output_scheduler
            .scheduled_video_queue
            .rebuffer_audio_clock_resume_decision(
                &output_scheduler.pending_start_audio,
                resume_audio_played_until,
                audio_output_buffered_until_nsecs,
                rebuffer_anchor
                    .is_some_and(|anchor| anchor.reset_to_video_when_decoded_queue_misses_anchor),
            )
    }
    .unwrap_or(AudioClockResumeDecision {
        timeline_nsecs: fallback_timeline_nsecs,
        reset_audio_to_video: false,
    });
    let resume_audio_output_buffered_until_nsecs =
        audio_output_buffered_until_for_resume(resume_decision, audio_output_buffered_until_nsecs);
    let dropped_resume_video_frames = output_scheduler
        .scheduled_video_queue
        .discard_before(resume_decision.timeline_nsecs);
    if dropped_resume_video_frames > 0 {
        tracing::debug!(
            session_id = ?session_id,
            dropped_resume_video_frames,
            previous_audio_played_until,
            rebuffer_anchor_timeline_nsecs = rebuffer_anchor.map(|anchor| anchor.timeline_nsecs),
            resume_timeline_nsecs = resume_decision.timeline_nsecs,
            reset_audio_to_video = resume_decision.reset_audio_to_video,
            output_scheduler.playback_output_state = ?output_scheduler.playback_output_state,
            "discarded decoded FFmpeg video before output gate resume"
        );
    }

    let waterline = if output_scheduler
        .playback_output_state
        .first_video_frame_pending()
    {
        output_scheduler
            .scheduled_video_queue
            .initial_playback_resume_waterline(
                &output_scheduler.pending_start_audio,
                resume_decision.timeline_nsecs,
                demux_watermark(),
                needs_prefetch,
                true,
            )
    } else {
        output_scheduler
            .scheduled_video_queue
            .rebuffer_playback_resume_waterline(
                &output_scheduler.pending_start_audio,
                resume_decision.timeline_nsecs,
                demux_watermark(),
                resume_audio_output_buffered_until_nsecs,
                needs_prefetch,
                true,
            )
    };
    if waterline.ready()
        && output_scheduler
            .playback_output_state
            .first_video_frame_pending()
    {
        output_scheduler.set_state(PlaybackOutputState::Ready);
        let Some(first_video) = present_first_queued_video_frame(
            output_scheduler,
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            |first_video, output_scheduler| {
                if first_video.timeline_nsecs > *current_start_position_nsecs {
                    *current_start_position_nsecs = first_video.timeline_nsecs;
                    scheduler.reset(first_video.timeline_nsecs);
                    subtitle_pipeline.reset_cues_for_position(first_video.timeline_nsecs);
                    buffered_reporter.reset_to(
                        nsecs_to_seconds(first_video.timeline_nsecs),
                        session_id,
                        event_tx,
                    );
                }
                output.reset_clock(first_video.timeline_nsecs);
                tracing::debug!(
                    session_id = ?session_id,
                    pts = first_video.timeline_nsecs,
                    output_scheduler.scheduled_video_queue =
                        output_scheduler.scheduled_video_queue.len(),
                    decoded_video_range =
                        ?output_scheduler.scheduled_video_queue.range_nsecs(),
                    queued_video_ms =
                        output_scheduler.scheduled_video_queue.duration().as_secs_f64() * 1000.0,
                    target_ms = waterline.target_nsecs as f64 / 1_000_000.0,
                    demux_min_ms = ?waterline
                        .demux_min_forward_nsecs
                        .map(|duration| duration as f64 / 1_000_000.0),
                    reset_audio_to_video = resume_decision.reset_audio_to_video,
                    "presenting first FFmpeg video frame from output gate"
                );
            },
        ) else {
            return Ok(OutputGateResumeStatus::Waiting);
        };
        let audio_flush_until_timeline_nsecs = output_scheduler
            .scheduled_video_queue
            .buffered_until_nsecs()
            .unwrap_or_else(|| {
                first_video
                    .timeline_nsecs
                    .saturating_add(first_video.duration_nsecs)
            });
        flush_pending_start_audio(
            &mut output_scheduler.pending_start_audio,
            output,
            first_video.timeline_nsecs,
            audio_flush_until_timeline_nsecs,
            control,
            &mut output_scheduler.scheduled_video_queue,
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            subtitle_pipeline,
            buffered_reporter,
        )?;
        return Ok(OutputGateResumeStatus::Resumed);
    }
    if waterline.ready()
        && output_scheduler.playback_output_state.rebuffering()
        && output_scheduler.finish_rebuffer_if_ready(waterline, session_id)
    {
        if resume_decision.reset_audio_to_video {
            output.reset_clock(resume_decision.timeline_nsecs);
        }
        let audio_flush_until_timeline_nsecs = output_scheduler
            .scheduled_video_queue
            .buffered_until_nsecs()
            .unwrap_or(resume_decision.timeline_nsecs);
        flush_pending_start_audio(
            &mut output_scheduler.pending_start_audio,
            output,
            resume_decision.timeline_nsecs,
            audio_flush_until_timeline_nsecs,
            control,
            &mut output_scheduler.scheduled_video_queue,
            session_id,
            vo_queue,
            frame_presented,
            position_reporter,
            event_tx,
            subtitle_pipeline,
            buffered_reporter,
        )?;
        control.set_output_rebuffer_paused(false);
        output_scheduler.set_state(PlaybackOutputState::Playing);
        return Ok(OutputGateResumeStatus::Resumed);
    }
    if !waterline.ready() {
        let demux_watermark = demux_watermark();
        output_scheduler
            .scheduled_video_queue
            .log_resume_waterline_wait(
                session_id,
                "output_gate",
                output_scheduler.playback_output_state,
                resume_decision.timeline_nsecs,
                &output_scheduler.pending_start_audio,
                waterline,
                demux_watermark,
            );
    }
    if waterline.decoded_ready() && !waterline.demux_ready {
        return Ok(OutputGateResumeStatus::WaitingForDemux);
    }
    Ok(OutputGateResumeStatus::Waiting)
}
