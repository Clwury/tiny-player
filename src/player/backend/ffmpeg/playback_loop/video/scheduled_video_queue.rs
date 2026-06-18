use std::{collections::VecDeque, time::Duration};

use crate::player::render_host::PlaybackSessionId;

use super::super::{
    queued_video_duration, queued_video_limit_duration, queued_video_limit_frames,
    queued_video_limit_reached, queued_video_target_duration, queued_video_target_frames,
};
use super::output_rebuffer::{
    AudioClockResumeDecision, PlaybackOutputState, PlaybackResumeWaterline,
    initial_audio_clock_resume_decision, initial_playback_resume_waterline,
    log_playback_resume_waterline_wait, rebuffer_audio_clock_resume_decision,
    rebuffer_playback_resume_waterline_with_resource_pressure,
    video_decode_should_skip_nonref_for_pressure,
};
use super::pending_audio_queue::PendingStartAudio;
use super::{
    AUDIO_CLOCK_VIDEO_PRESENT_LEAD, AUDIO_OUTPUT_VIDEO_LEAD_DURATION, DemuxReaderWatermark,
    LATE_VIDEO_DROP_TOLERANCE, PlaybackScheduler, QueuedVideoFrame,
    VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION, duration_nsecs,
};

#[derive(Default)]
pub(in crate::player::backend::ffmpeg) struct ScheduledVideoQueue {
    frames: VecDeque<QueuedVideoFrame>,
}

#[derive(Default)]
pub(in crate::player::backend::ffmpeg) struct AudioClockedVideoPopResult {
    pub(in crate::player::backend::ffmpeg) frame: Option<QueuedVideoFrame>,
    pub(in crate::player::backend::ffmpeg) dropped_frames: usize,
}

pub(in crate::player::backend::ffmpeg) fn discard_queued_video_before(
    queued_video_frames: &mut VecDeque<QueuedVideoFrame>,
    timeline_nsecs: u64,
) -> usize {
    let mut dropped = 0usize;
    while queued_video_frames.front().is_some_and(|frame| {
        frame.timeline_nsecs.saturating_add(frame.duration_nsecs) <= timeline_nsecs
    }) {
        queued_video_frames.pop_front();
        dropped = dropped.saturating_add(1);
    }
    dropped
}

pub(in crate::player::backend::ffmpeg) fn push_queued_video_frame(
    queued_video_frames: &mut VecDeque<QueuedVideoFrame>,
    frame: QueuedVideoFrame,
) {
    if queued_video_frames
        .back()
        .is_none_or(|queued| queued.timeline_nsecs <= frame.timeline_nsecs)
    {
        queued_video_frames.push_back(frame);
        return;
    }

    let insert_at = queued_video_frames
        .iter()
        .position(|queued| queued.timeline_nsecs > frame.timeline_nsecs)
        .unwrap_or(queued_video_frames.len());
    queued_video_frames.insert(insert_at, frame);
}

pub(in crate::player::backend::ffmpeg) fn queued_video_buffered_until_nsecs(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
) -> Option<u64> {
    queued_video_frames
        .back()
        .map(|frame| frame.timeline_nsecs.saturating_add(frame.duration_nsecs))
}

pub(in crate::player::backend::ffmpeg) fn queued_video_buffered_until_from_nsecs(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    timeline_nsecs: u64,
) -> Option<u64> {
    let buffered_until = queued_video_buffered_until_nsecs(queued_video_frames)?;
    if queued_video_frames
        .front()
        .is_some_and(|frame| timeline_nsecs <= frame.timeline_nsecs)
    {
        return Some(buffered_until);
    }
    queued_video_frames
        .iter()
        .any(|frame| {
            let frame_end_nsecs = frame.timeline_nsecs.saturating_add(frame.duration_nsecs);
            frame.timeline_nsecs <= timeline_nsecs && frame_end_nsecs > timeline_nsecs
        })
        .then_some(buffered_until)
}

pub(in crate::player::backend::ffmpeg) fn queued_video_range_nsecs(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
) -> Option<(u64, u64)> {
    let start_nsecs = queued_video_frames.front()?.timeline_nsecs;
    let end_nsecs = queued_video_buffered_until_nsecs(queued_video_frames)?;
    Some((start_nsecs, end_nsecs))
}

pub(in crate::player::backend::ffmpeg) fn queued_video_forward_nsecs_from(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    timeline_nsecs: u64,
) -> Option<u64> {
    queued_video_buffered_until_from_nsecs(queued_video_frames, timeline_nsecs)
        .map(|buffered_until| buffered_until.saturating_sub(timeline_nsecs))
}

pub(in crate::player::backend::ffmpeg) fn video_output_rebuffer_low_water(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    played_until_nsecs: u64,
) -> bool {
    if queued_video_frames.is_empty() {
        return true;
    }

    queued_video_forward_nsecs_from(queued_video_frames, played_until_nsecs)
        .is_none_or(|forward| forward <= duration_nsecs(VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION))
}

pub(in crate::player::backend::ffmpeg) fn audio_output_video_lead_until_from_nsecs(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    timeline_nsecs: u64,
) -> Option<u64> {
    queued_video_buffered_until_from_nsecs(queued_video_frames, timeline_nsecs)
        .map(|timeline| timeline.saturating_add(duration_nsecs(AUDIO_OUTPUT_VIDEO_LEAD_DURATION)))
}

impl ScheduledVideoQueue {
    pub(in crate::player::backend::ffmpeg) fn clear(&mut self) {
        self.frames.clear();
    }

    pub(in crate::player::backend::ffmpeg) fn len(&self) -> usize {
        self.frames.len()
    }

    pub(in crate::player::backend::ffmpeg) fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub(in crate::player::backend::ffmpeg) fn front_ready_for(
        &self,
        scheduler: &PlaybackScheduler,
    ) -> bool {
        self.frames
            .front()
            .is_some_and(|frame| scheduler.ready_for(frame.timeline_nsecs))
    }

    pub(in crate::player::backend::ffmpeg) fn pop_front(&mut self) -> Option<QueuedVideoFrame> {
        self.frames.pop_front()
    }

    pub(in crate::player::backend::ffmpeg) fn duration_nsecs(&self) -> u64 {
        duration_nsecs(queued_video_duration(&self.frames))
    }

    pub(in crate::player::backend::ffmpeg) fn duration(&self) -> Duration {
        queued_video_duration(&self.frames)
    }

    pub(in crate::player::backend::ffmpeg) fn range_nsecs(&self) -> Option<(u64, u64)> {
        queued_video_range_nsecs(&self.frames)
    }

    pub(in crate::player::backend::ffmpeg) fn buffered_until_from_nsecs(
        &self,
        timeline_nsecs: u64,
    ) -> Option<u64> {
        queued_video_buffered_until_from_nsecs(&self.frames, timeline_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn forward_nsecs_from(
        &self,
        timeline_nsecs: u64,
    ) -> Option<u64> {
        queued_video_forward_nsecs_from(&self.frames, timeline_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn low_water(&self, played_until_nsecs: u64) -> bool {
        video_output_rebuffer_low_water(&self.frames, played_until_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn limit_reached(
        &self,
        needs_subtitle_prefetch: bool,
    ) -> bool {
        queued_video_limit_reached(&self.frames, needs_subtitle_prefetch)
    }

    pub(in crate::player::backend::ffmpeg) fn limit_frames(
        &self,
        needs_subtitle_prefetch: bool,
    ) -> usize {
        queued_video_limit_frames(&self.frames, needs_subtitle_prefetch)
    }

    pub(in crate::player::backend::ffmpeg) fn limit_duration(
        &self,
        needs_subtitle_prefetch: bool,
    ) -> Duration {
        queued_video_limit_duration(&self.frames, needs_subtitle_prefetch)
    }

    pub(in crate::player::backend::ffmpeg) fn target_frames(
        &self,
        needs_subtitle_prefetch: bool,
    ) -> usize {
        queued_video_target_frames(&self.frames, needs_subtitle_prefetch)
    }

    pub(in crate::player::backend::ffmpeg) fn target_duration(
        &self,
        needs_subtitle_prefetch: bool,
    ) -> Duration {
        queued_video_target_duration(&self.frames, needs_subtitle_prefetch)
    }

    pub(in crate::player::backend::ffmpeg) fn skip_nonref_for_pressure(
        &self,
        output_state: PlaybackOutputState,
        played_until_nsecs: Option<u64>,
        has_audio_output: bool,
        audio_output_pending_nsecs: Option<u64>,
        skip_nonref_active: bool,
    ) -> bool {
        video_decode_should_skip_nonref_for_pressure(
            output_state,
            &self.frames,
            played_until_nsecs,
            has_audio_output,
            audio_output_pending_nsecs,
            skip_nonref_active,
        )
    }

    pub(in crate::player::backend::ffmpeg) fn audio_output_lead_until_from_nsecs(
        &self,
        timeline_nsecs: u64,
    ) -> Option<u64> {
        audio_output_video_lead_until_from_nsecs(&self.frames, timeline_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn audio_clock_wait_duration(
        &self,
        played_until_nsecs: u64,
    ) -> Option<Duration> {
        audio_clocked_video_wait_duration(&self.frames, played_until_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn push_queued(&mut self, frame: QueuedVideoFrame) {
        push_queued_video_frame(&mut self.frames, frame);
    }

    pub(in crate::player::backend::ffmpeg) fn discard_before(
        &mut self,
        timeline_nsecs: u64,
    ) -> usize {
        discard_queued_video_before(&mut self.frames, timeline_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn initial_audio_clock_resume_decision(
        &self,
        pending_audio: &PendingStartAudio,
        played_until_nsecs: u64,
    ) -> Option<AudioClockResumeDecision> {
        initial_audio_clock_resume_decision(&self.frames, pending_audio, played_until_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn rebuffer_audio_clock_resume_decision(
        &self,
        pending_audio: &PendingStartAudio,
        played_until_nsecs: u64,
        audio_output_buffered_until_nsecs: Option<u64>,
        reset_to_video_when_decoded_queue_misses_anchor: bool,
    ) -> Option<AudioClockResumeDecision> {
        rebuffer_audio_clock_resume_decision(
            &self.frames,
            pending_audio,
            played_until_nsecs,
            audio_output_buffered_until_nsecs,
            reset_to_video_when_decoded_queue_misses_anchor,
        )
    }

    pub(in crate::player::backend::ffmpeg) fn initial_playback_resume_waterline(
        &self,
        pending_audio: &PendingStartAudio,
        resume_timeline_nsecs: u64,
        demux_watermark: DemuxReaderWatermark,
        needs_prefetch: bool,
        has_audio_output: bool,
    ) -> PlaybackResumeWaterline {
        initial_playback_resume_waterline(
            &self.frames,
            pending_audio,
            resume_timeline_nsecs,
            demux_watermark,
            needs_prefetch,
            has_audio_output,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::player::backend::ffmpeg) fn rebuffer_playback_resume_waterline_with_resource_pressure(
        &self,
        pending_audio: &PendingStartAudio,
        resume_timeline_nsecs: u64,
        demux_watermark: DemuxReaderWatermark,
        audio_output_buffered_until_nsecs: Option<u64>,
        needs_prefetch: bool,
        has_audio_output: bool,
        output_resource_pressure: bool,
    ) -> PlaybackResumeWaterline {
        rebuffer_playback_resume_waterline_with_resource_pressure(
            &self.frames,
            pending_audio,
            resume_timeline_nsecs,
            demux_watermark,
            audio_output_buffered_until_nsecs,
            needs_prefetch,
            has_audio_output,
            output_resource_pressure,
        )
    }

    pub(in crate::player::backend::ffmpeg) fn pop_audio_clocked_frame(
        &mut self,
        played_until_nsecs: u64,
    ) -> AudioClockedVideoPopResult {
        pop_audio_clocked_video_frame_with_policy(&mut self.frames, played_until_nsecs)
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::player::backend::ffmpeg) fn log_resume_waterline_wait(
        &self,
        session_id: PlaybackSessionId,
        context: &'static str,
        playback_output_state: PlaybackOutputState,
        resume_timeline_nsecs: u64,
        pending_audio: &PendingStartAudio,
        waterline: PlaybackResumeWaterline,
        demux_watermark: DemuxReaderWatermark,
    ) {
        log_playback_resume_waterline_wait(
            session_id,
            context,
            playback_output_state,
            resume_timeline_nsecs,
            &self.frames,
            pending_audio,
            waterline,
            demux_watermark,
        );
    }
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn pop_audio_clocked_video_frame(
    queued_video_frames: &mut VecDeque<QueuedVideoFrame>,
    played_until_nsecs: u64,
) -> Option<QueuedVideoFrame> {
    pop_audio_clocked_video_frame_with_policy(queued_video_frames, played_until_nsecs).frame
}

pub(in crate::player::backend::ffmpeg) fn pop_audio_clocked_video_frame_with_policy(
    queued_video_frames: &mut VecDeque<QueuedVideoFrame>,
    played_until_nsecs: u64,
) -> AudioClockedVideoPopResult {
    let mut result = AudioClockedVideoPopResult::default();
    while queued_video_frames.front().is_some_and(|frame| {
        should_drop_late_video_frame(
            frame.timeline_nsecs,
            frame.duration_nsecs,
            played_until_nsecs,
        )
    }) {
        queued_video_frames.pop_front();
        result.dropped_frames = result.dropped_frames.saturating_add(1);
    }

    let mut due_frame = None;
    while queued_video_frames
        .front()
        .is_some_and(|frame| frame.timeline_nsecs <= played_until_nsecs)
    {
        if due_frame.is_some() {
            result.dropped_frames = result.dropped_frames.saturating_add(1);
        }
        due_frame = Some(
            queued_video_frames
                .pop_front()
                .expect("queued video frame checked above"),
        );
    }
    if due_frame.is_none()
        && queued_video_frame_ready_for_audio_clock(queued_video_frames, played_until_nsecs)
    {
        due_frame = queued_video_frames.pop_front();
    }
    result.frame = due_frame;
    result
}

pub(in crate::player::backend::ffmpeg) fn queued_video_frame_ready_for_audio_clock(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    played_until_nsecs: u64,
) -> bool {
    let Some(frame) = queued_video_frames.front() else {
        return false;
    };
    frame.timeline_nsecs
        <= played_until_nsecs.saturating_add(audio_clock_video_present_lead_nsecs(frame))
}

pub(in crate::player::backend::ffmpeg) fn audio_clocked_video_wait_duration(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    played_until_nsecs: u64,
) -> Option<Duration> {
    let frame = queued_video_frames.front()?;
    let ready_at_nsecs = frame
        .timeline_nsecs
        .saturating_sub(audio_clock_video_present_lead_nsecs(frame));
    Some(Duration::from_nanos(
        ready_at_nsecs.saturating_sub(played_until_nsecs),
    ))
}

fn audio_clock_video_present_lead_nsecs(frame: &QueuedVideoFrame) -> u64 {
    duration_nsecs(AUDIO_CLOCK_VIDEO_PRESENT_LEAD).min(frame.duration_nsecs)
}

pub(in crate::player::backend::ffmpeg) fn should_drop_late_video_frame(
    frame_timeline_nsecs: u64,
    frame_duration_nsecs: u64,
    played_until_nsecs: u64,
) -> bool {
    let late_cutoff = frame_timeline_nsecs
        .saturating_add(frame_duration_nsecs)
        .saturating_add(duration_nsecs(LATE_VIDEO_DROP_TOLERANCE));
    late_cutoff <= played_until_nsecs
}
