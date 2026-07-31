use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use ffmpeg_sys_next as ffi;

use crate::player::render_host::PlaybackSessionId;

use super::super::{
    queued_video_duration, queued_video_limit_duration, queued_video_limit_frames,
    queued_video_limit_reached, queued_video_range_span, queued_video_target_duration,
    queued_video_target_frames,
};
use super::output_rebuffer::{
    AudioClockResumeDecision, InitialOutputSyncDecision, PlaybackOutputState,
    PlaybackResumeWaterline, initial_output_sync_decision, initial_playback_resume_waterline,
    log_playback_resume_waterline_wait, rebuffer_audio_clock_resume_decision,
    rebuffer_playback_resume_waterline_for_decision, video_decode_should_skip_nonref_for_pressure,
};
use super::pending_audio_queue::PendingStartAudio;
use super::{
    AUDIO_CLOCK_VIDEO_PRESENT_LEAD, AUDIO_OUTPUT_VIDEO_LEAD_DURATION, DemuxReaderWatermark,
    LATE_VIDEO_DROP_TOLERANCE, PlaybackScheduler, QueuedVideoFrame,
    VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION, duration_nsecs,
};

pub(in crate::player::backend::ffmpeg) struct ScheduledVideoQueue {
    shared: Arc<ScheduledVideoQueueShared>,
    recent_coordinator_stall: Option<CoordinatorStall>,
    last_prebuffer_limit_log_at: Option<Instant>,
    suppressed_prebuffer_limit_logs: u64,
}

struct ScheduledVideoQueueShared {
    frames: Mutex<VecDeque<QueuedVideoFrame>>,
    changed: Condvar,
    deadline_service_attached: AtomicBool,
    deadline_service_active: AtomicBool,
    presentation_generation: AtomicU64,
    presentation_session_id: AtomicU64,
    admission_gate: Mutex<()>,
    scheduler_dropped_video_frames: AtomicU64,
}

#[derive(Clone)]
pub(in crate::player::backend::ffmpeg) struct ScheduledVideoDeadlineQueue {
    shared: Arc<ScheduledVideoQueueShared>,
}

pub(in crate::player::backend::ffmpeg) struct DeadlineVideoFrame {
    pub(in crate::player::backend::ffmpeg) frame: QueuedVideoFrame,
    generation: u64,
    session_id: PlaybackSessionId,
}

impl Default for ScheduledVideoQueue {
    fn default() -> Self {
        Self {
            shared: Arc::new(ScheduledVideoQueueShared {
                frames: Mutex::new(VecDeque::new()),
                changed: Condvar::new(),
                deadline_service_attached: AtomicBool::new(false),
                deadline_service_active: AtomicBool::new(false),
                presentation_generation: AtomicU64::new(1),
                presentation_session_id: AtomicU64::new(PlaybackSessionId::default().0),
                admission_gate: Mutex::new(()),
                scheduler_dropped_video_frames: AtomicU64::new(0),
            }),
            recent_coordinator_stall: None,
            last_prebuffer_limit_log_at: None,
            suppressed_prebuffer_limit_logs: 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct CoordinatorStall {
    elapsed: Duration,
    completed_at: Instant,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct RecentCoordinatorStall {
    pub(in crate::player::backend::ffmpeg) elapsed: Duration,
    pub(in crate::player::backend::ffmpeg) age: Duration,
}

#[derive(Default)]
pub(in crate::player::backend::ffmpeg) struct AudioClockedVideoPopResult {
    pub(in crate::player::backend::ffmpeg) frame: Option<QueuedVideoFrame>,
    pub(in crate::player::backend::ffmpeg) dropped_frames: usize,
    pub(in crate::player::backend::ffmpeg) forced_for_minimum_frame_rate: bool,
}

const VIDEO_CONTIGUITY_MIN_GAP_NSECS: u64 = 200_000_000;
const VIDEO_CONTIGUITY_MAX_GAP_NSECS: u64 = 500_000_000;
const VIDEO_CONTIGUITY_GAP_FRAME_MULTIPLIER: u64 = 3;
const COORDINATOR_STALL_RECORD_THRESHOLD: Duration = Duration::from_millis(20);
const COORDINATOR_STALL_RECENT_WINDOW: Duration = Duration::from_secs(2);
const PREBUFFER_LIMIT_LOG_INTERVAL: Duration = Duration::from_millis(500);
// Rational FFmpeg timestamps converted to integer nanoseconds can differ by a few
// nanoseconds at an otherwise exact continuity boundary.
pub(in crate::player::backend::ffmpeg) const VIDEO_TIMESTAMP_ROUNDING_TOLERANCE_NSECS: u64 = 1_000;
// mpv's calculate_frame_duration() accepts 3 ms of mux timestamp rounding plus
// 0.1 ms of slack before treating adjacent frame timing as discontinuous. Keep
// strict startup lookahead equally tolerant: this is far below a frame period,
// but covers coarse source time bases such as the 41.668 us jitter seen at 30 fps.
const VIDEO_STRICT_CONTINUITY_MAX_TIMESTAMP_JITTER_NSECS: u64 = 3_100_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct QueuedVideoContinuity {
    pub(in crate::player::backend::ffmpeg) buffered_until_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) forward_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) largest_gap_nsecs: Option<u64>,
}

pub(in crate::player::backend::ffmpeg) fn queued_video_continuity_gap_threshold_nsecs(
    frame_duration_nsecs: u64,
) -> u64 {
    frame_duration_nsecs
        .saturating_mul(VIDEO_CONTIGUITY_GAP_FRAME_MULTIPLIER)
        .clamp(
            VIDEO_CONTIGUITY_MIN_GAP_NSECS,
            VIDEO_CONTIGUITY_MAX_GAP_NSECS,
        )
}

pub(in crate::player::backend::ffmpeg) fn video_timestamp_gap_within_threshold(
    gap_nsecs: u64,
    max_gap_nsecs: u64,
) -> bool {
    gap_nsecs <= max_gap_nsecs.saturating_add(VIDEO_TIMESTAMP_ROUNDING_TOLERANCE_NSECS)
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
    let start_index = queued_video_contiguous_start_index(queued_video_frames, timeline_nsecs)?;
    let max_gap_nsecs = queued_video_continuity_gap_threshold_nsecs(
        queued_video_frames.get(start_index)?.duration_nsecs,
    );
    queued_video_contiguous_buffered_until_from_nsecs(
        queued_video_frames,
        timeline_nsecs,
        max_gap_nsecs,
    )
}

pub(in crate::player::backend::ffmpeg) fn queued_video_contiguous_buffered_until_from_nsecs(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    timeline_nsecs: u64,
    max_gap_nsecs: u64,
) -> Option<u64> {
    queued_video_contiguous_buffered_until_from_nsecs_with_gap(
        queued_video_frames,
        timeline_nsecs,
        Some(max_gap_nsecs),
    )
}

fn queued_video_contiguous_frame_count_from_nsecs(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    timeline_nsecs: u64,
) -> usize {
    let Some(start_index) =
        queued_video_contiguous_start_index(queued_video_frames, timeline_nsecs)
    else {
        return 0;
    };
    let Some(first) = queued_video_frames.get(start_index) else {
        return 0;
    };
    let max_gap_nsecs = queued_video_continuity_gap_threshold_nsecs(first.duration_nsecs);
    let mut buffered_until = first.timeline_nsecs.saturating_add(first.duration_nsecs);
    let mut count = 1usize;
    for frame in queued_video_frames.iter().skip(start_index + 1) {
        let gap_nsecs = frame.timeline_nsecs.saturating_sub(buffered_until);
        if !video_timestamp_gap_within_threshold(gap_nsecs, max_gap_nsecs) {
            break;
        }
        buffered_until =
            buffered_until.max(frame.timeline_nsecs.saturating_add(frame.duration_nsecs));
        count = count.saturating_add(1);
    }
    count
}

fn queued_video_contiguous_buffered_until_from_nsecs_with_gap(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    timeline_nsecs: u64,
    fixed_max_gap_nsecs: Option<u64>,
) -> Option<u64> {
    let start_index = queued_video_contiguous_start_index(queued_video_frames, timeline_nsecs)?;
    let first = queued_video_frames.get(start_index)?;
    let mut buffered_until = first.timeline_nsecs.saturating_add(first.duration_nsecs);
    let mut previous_duration_nsecs = first.duration_nsecs;

    for frame in queued_video_frames.iter().skip(start_index + 1) {
        let gap_nsecs = frame.timeline_nsecs.saturating_sub(buffered_until);
        let max_gap_nsecs = fixed_max_gap_nsecs.unwrap_or_else(|| {
            queued_video_continuity_gap_threshold_nsecs(previous_duration_nsecs)
        });
        if !video_timestamp_gap_within_threshold(gap_nsecs, max_gap_nsecs) {
            break;
        }
        buffered_until =
            buffered_until.max(frame.timeline_nsecs.saturating_add(frame.duration_nsecs));
        previous_duration_nsecs = frame.duration_nsecs;
    }

    Some(buffered_until)
}

fn queued_video_contiguous_start_index(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    timeline_nsecs: u64,
) -> Option<usize> {
    let first = queued_video_frames.front()?;
    if timeline_nsecs <= first.timeline_nsecs {
        return Some(0);
    }

    queued_video_frames.iter().position(|frame| {
        let frame_end_nsecs = frame.timeline_nsecs.saturating_add(frame.duration_nsecs);
        frame.timeline_nsecs <= timeline_nsecs && frame_end_nsecs > timeline_nsecs
    })
}

pub(in crate::player::backend::ffmpeg) fn queued_video_largest_gap_nsecs(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
) -> Option<u64> {
    let mut frames = queued_video_frames.iter();
    let mut previous = frames.next()?;
    let mut largest_gap_nsecs = None::<u64>;
    for frame in frames {
        let previous_end_nsecs = previous
            .timeline_nsecs
            .saturating_add(previous.duration_nsecs);
        let gap_nsecs = frame.timeline_nsecs.saturating_sub(previous_end_nsecs);
        if gap_nsecs > 0 {
            largest_gap_nsecs = Some(largest_gap_nsecs.unwrap_or_default().max(gap_nsecs));
        }
        previous = frame;
    }
    largest_gap_nsecs
}

pub(in crate::player::backend::ffmpeg) fn queued_video_continuity_from_nsecs(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    timeline_nsecs: u64,
) -> QueuedVideoContinuity {
    let buffered_until_nsecs =
        queued_video_buffered_until_from_nsecs(queued_video_frames, timeline_nsecs);
    QueuedVideoContinuity {
        buffered_until_nsecs,
        forward_nsecs: buffered_until_nsecs
            .map(|buffered_until| buffered_until.saturating_sub(timeline_nsecs)),
        largest_gap_nsecs: queued_video_largest_gap_nsecs(queued_video_frames),
    }
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
    queued_video_continuity_from_nsecs(queued_video_frames, timeline_nsecs).forward_nsecs
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

impl ScheduledVideoQueueShared {
    fn frames(&self) -> MutexGuard<'_, VecDeque<QueuedVideoFrame>> {
        self.frames
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn advance_presentation_generation(&self) -> u64 {
        self.presentation_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1)
    }
}

impl ScheduledVideoQueue {
    pub(in crate::player::backend::ffmpeg) fn clear(&mut self) {
        self.clear_for_presentation_session(None);
    }

    pub(in crate::player::backend::ffmpeg) fn clear_and_bind_presentation_session(
        &mut self,
        session_id: PlaybackSessionId,
    ) {
        self.clear_for_presentation_session(Some(session_id));
    }

    fn clear_for_presentation_session(&mut self, session_id: Option<PlaybackSessionId>) {
        let _admission = self
            .shared
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.shared.advance_presentation_generation();
        if let Some(session_id) = session_id {
            self.shared
                .presentation_session_id
                .store(session_id.0, Ordering::Release);
        }
        self.shared.frames().clear();
        self.shared
            .scheduler_dropped_video_frames
            .store(0, Ordering::Release);
        self.shared.changed.notify_all();
        self.recent_coordinator_stall = None;
        self.last_prebuffer_limit_log_at = None;
        self.suppressed_prebuffer_limit_logs = 0;
    }

    pub(in crate::player::backend::ffmpeg) fn len(&self) -> usize {
        self.shared.frames().len()
    }

    pub(in crate::player::backend::ffmpeg) fn is_empty(&self) -> bool {
        self.shared.frames().is_empty()
    }

    pub(in crate::player::backend::ffmpeg) fn with_frames<R>(
        &self,
        inspect: impl FnOnce(&VecDeque<QueuedVideoFrame>) -> R,
    ) -> R {
        inspect(&self.shared.frames())
    }

    pub(in crate::player::backend::ffmpeg) fn contiguous_frame_count_from(
        &self,
        timeline_nsecs: u64,
    ) -> usize {
        self.with_frames(|frames| {
            queued_video_contiguous_frame_count_from_nsecs(frames, timeline_nsecs)
        })
    }

    pub(in crate::player::backend::ffmpeg) fn front_ready_for(
        &self,
        scheduler: &PlaybackScheduler,
    ) -> bool {
        self.with_frames(|frames| {
            frames
                .front()
                .is_some_and(|frame| scheduler.ready_for(frame.timeline_nsecs))
        })
    }

    pub(in crate::player::backend::ffmpeg) fn pop_front(&mut self) -> Option<QueuedVideoFrame> {
        let frame = self.shared.frames().pop_front();
        if frame.is_some() {
            self.shared.changed.notify_all();
        }
        frame
    }

    pub(in crate::player::backend::ffmpeg) fn duration_nsecs(&self) -> u64 {
        self.with_frames(|frames| duration_nsecs(queued_video_duration(frames)))
    }

    pub(in crate::player::backend::ffmpeg) fn duration(&self) -> Duration {
        self.with_frames(queued_video_duration)
    }

    pub(in crate::player::backend::ffmpeg) fn range_span_nsecs(&self) -> u64 {
        self.with_frames(|frames| duration_nsecs(queued_video_range_span(frames)))
    }

    pub(in crate::player::backend::ffmpeg) fn range_nsecs(&self) -> Option<(u64, u64)> {
        self.with_frames(queued_video_range_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn back_timing_nsecs(&self) -> Option<(u64, u64)> {
        self.with_frames(|frames| {
            frames
                .back()
                .map(|frame| (frame.timeline_nsecs, frame.duration_nsecs))
        })
    }

    pub(in crate::player::backend::ffmpeg) fn extend_back_duration_to(
        &mut self,
        timeline_nsecs: u64,
    ) -> Option<(u64, u64)> {
        let mut frames = self.shared.frames();
        let frame = frames.back_mut()?;
        let extended_duration_nsecs = timeline_nsecs.checked_sub(frame.timeline_nsecs)?;
        if extended_duration_nsecs <= frame.duration_nsecs {
            return None;
        }
        let previous_duration_nsecs = frame.duration_nsecs;
        frame.duration_nsecs = extended_duration_nsecs;
        drop(frames);
        self.shared.changed.notify_all();
        Some((previous_duration_nsecs, extended_duration_nsecs))
    }

    pub(in crate::player::backend::ffmpeg) fn buffered_until_from_nsecs(
        &self,
        timeline_nsecs: u64,
    ) -> Option<u64> {
        self.with_frames(|frames| queued_video_buffered_until_from_nsecs(frames, timeline_nsecs))
    }

    pub(in crate::player::backend::ffmpeg) fn forward_nsecs_from(
        &self,
        timeline_nsecs: u64,
    ) -> Option<u64> {
        self.with_frames(|frames| queued_video_forward_nsecs_from(frames, timeline_nsecs))
    }

    pub(in crate::player::backend::ffmpeg) fn strict_forward_nsecs_from(
        &self,
        timeline_nsecs: u64,
    ) -> Option<u64> {
        self.with_frames(|frames| {
            queued_video_contiguous_buffered_until_from_nsecs(
                frames,
                timeline_nsecs,
                VIDEO_STRICT_CONTINUITY_MAX_TIMESTAMP_JITTER_NSECS,
            )
            .map(|until| until.saturating_sub(timeline_nsecs))
        })
    }

    pub(in crate::player::backend::ffmpeg) fn continuity_from_nsecs(
        &self,
        timeline_nsecs: u64,
    ) -> QueuedVideoContinuity {
        self.with_frames(|frames| queued_video_continuity_from_nsecs(frames, timeline_nsecs))
    }

    pub(in crate::player::backend::ffmpeg) fn largest_gap_nsecs(&self) -> Option<u64> {
        self.with_frames(queued_video_largest_gap_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn low_water(&self, played_until_nsecs: u64) -> bool {
        self.with_frames(|frames| video_output_rebuffer_low_water(frames, played_until_nsecs))
    }

    pub(in crate::player::backend::ffmpeg) fn limit_reached(
        &self,
        needs_subtitle_prefetch: bool,
    ) -> bool {
        self.with_frames(|frames| queued_video_limit_reached(frames, needs_subtitle_prefetch))
    }

    pub(in crate::player::backend::ffmpeg) fn limit_frames(
        &self,
        needs_subtitle_prefetch: bool,
    ) -> usize {
        self.with_frames(|frames| queued_video_limit_frames(frames, needs_subtitle_prefetch))
    }

    pub(in crate::player::backend::ffmpeg) fn limit_duration(
        &self,
        needs_subtitle_prefetch: bool,
    ) -> Duration {
        self.with_frames(|frames| queued_video_limit_duration(frames, needs_subtitle_prefetch))
    }

    pub(in crate::player::backend::ffmpeg) fn target_frames(
        &self,
        needs_subtitle_prefetch: bool,
    ) -> usize {
        self.with_frames(|frames| queued_video_target_frames(frames, needs_subtitle_prefetch))
    }

    pub(in crate::player::backend::ffmpeg) fn target_duration(
        &self,
        needs_subtitle_prefetch: bool,
    ) -> Duration {
        self.with_frames(|frames| queued_video_target_duration(frames, needs_subtitle_prefetch))
    }

    pub(in crate::player::backend::ffmpeg) fn skip_nonref_for_pressure(
        &self,
        codec_id: ffi::AVCodecID,
        output_state: PlaybackOutputState,
        played_until_nsecs: Option<u64>,
        has_audio_output: bool,
        audio_output_pending_nsecs: Option<u64>,
        skip_nonref_active: bool,
    ) -> bool {
        self.with_frames(|frames| {
            video_decode_should_skip_nonref_for_pressure(
                codec_id,
                output_state,
                frames,
                played_until_nsecs,
                has_audio_output,
                audio_output_pending_nsecs,
                skip_nonref_active,
            )
        })
    }

    pub(in crate::player::backend::ffmpeg) fn audio_output_lead_until_from_nsecs(
        &self,
        timeline_nsecs: u64,
    ) -> Option<u64> {
        self.with_frames(|frames| audio_output_video_lead_until_from_nsecs(frames, timeline_nsecs))
    }

    pub(in crate::player::backend::ffmpeg) fn audio_clock_wait_duration(
        &self,
        played_until_nsecs: u64,
    ) -> Option<Duration> {
        self.with_frames(|frames| audio_clocked_video_wait_duration(frames, played_until_nsecs))
    }

    pub(in crate::player::backend::ffmpeg) fn push_queued(&mut self, frame: QueuedVideoFrame) {
        push_queued_video_frame(&mut self.shared.frames(), frame);
        self.shared.changed.notify_all();
    }

    pub(in crate::player::backend::ffmpeg) fn discard_before(
        &mut self,
        timeline_nsecs: u64,
    ) -> usize {
        let _admission = self
            .shared
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.shared.advance_presentation_generation();
        let dropped = discard_queued_video_before(&mut self.shared.frames(), timeline_nsecs);
        self.shared.changed.notify_all();
        dropped
    }

    pub(in crate::player::backend::ffmpeg) fn discard_at_or_after(
        &mut self,
        timeline_nsecs: u64,
    ) -> usize {
        let _admission = self
            .shared
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.shared.advance_presentation_generation();
        let mut frames = self.shared.frames();
        let original_len = frames.len();
        while frames
            .back()
            .is_some_and(|frame| frame.timeline_nsecs >= timeline_nsecs)
        {
            frames.pop_back();
        }
        if let Some(frame) = frames.back_mut() {
            let frame_end_nsecs = frame.timeline_nsecs.saturating_add(frame.duration_nsecs);
            if frame.timeline_nsecs < timeline_nsecs && frame_end_nsecs > timeline_nsecs {
                frame.duration_nsecs = timeline_nsecs.saturating_sub(frame.timeline_nsecs);
                frame.source_duration_nsecs = frame.source_duration_nsecs.min(frame.duration_nsecs);
            }
        }
        let dropped = original_len.saturating_sub(frames.len());
        drop(frames);
        self.shared.changed.notify_all();
        dropped
    }

    pub(in crate::player::backend::ffmpeg) fn append_from(&mut self, other: &mut Self) -> usize {
        if Arc::ptr_eq(&self.shared, &other.shared) {
            return 0;
        }
        let drained = {
            let _admission = other
                .shared
                .admission_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            other.shared.advance_presentation_generation();
            other.shared.frames().drain(..).collect::<Vec<_>>()
        };
        let appended = drained.len();
        {
            let mut frames = self.shared.frames();
            for frame in drained {
                push_queued_video_frame(&mut frames, frame);
            }
        }
        other.shared.changed.notify_all();
        self.shared.changed.notify_all();
        appended
    }

    pub(in crate::player::backend::ffmpeg) fn initial_output_sync_decision(
        &self,
        pending_audio: &PendingStartAudio,
        played_until_nsecs: u64,
    ) -> Option<InitialOutputSyncDecision> {
        self.with_frames(|frames| {
            initial_output_sync_decision(frames, pending_audio, played_until_nsecs)
        })
    }

    pub(in crate::player::backend::ffmpeg) fn rebuffer_audio_clock_resume_decision(
        &self,
        pending_audio: &PendingStartAudio,
        played_until_nsecs: u64,
        audio_output_buffered_until_nsecs: Option<u64>,
        audio_output_pending_nsecs: Option<u64>,
        reset_to_video_when_decoded_queue_misses_anchor: bool,
    ) -> Option<AudioClockResumeDecision> {
        self.with_frames(|frames| {
            rebuffer_audio_clock_resume_decision(
                frames,
                pending_audio,
                played_until_nsecs,
                audio_output_buffered_until_nsecs,
                audio_output_pending_nsecs,
                reset_to_video_when_decoded_queue_misses_anchor,
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::player::backend::ffmpeg) fn initial_playback_resume_waterline(
        &self,
        pending_audio: &PendingStartAudio,
        resume_timeline_nsecs: u64,
        delayed_audio_start_timeline_nsecs: Option<u64>,
        allow_initial_audio_gap_at_video_start: bool,
        demux_watermark: DemuxReaderWatermark,
        needs_prefetch: bool,
        has_audio_output: bool,
    ) -> PlaybackResumeWaterline {
        self.with_frames(|frames| {
            initial_playback_resume_waterline(
                frames,
                pending_audio,
                resume_timeline_nsecs,
                delayed_audio_start_timeline_nsecs,
                allow_initial_audio_gap_at_video_start,
                demux_watermark,
                needs_prefetch,
                has_audio_output,
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::player::backend::ffmpeg) fn rebuffer_playback_resume_waterline_for_decision(
        &self,
        pending_audio: &PendingStartAudio,
        resume_decision: AudioClockResumeDecision,
        demux_watermark: DemuxReaderWatermark,
        audio_output_buffered_until_nsecs: Option<u64>,
        needs_prefetch: bool,
        has_audio_output: bool,
        output_resource_pressure: bool,
    ) -> PlaybackResumeWaterline {
        self.with_frames(|frames| {
            rebuffer_playback_resume_waterline_for_decision(
                frames,
                pending_audio,
                resume_decision,
                demux_watermark,
                audio_output_buffered_until_nsecs,
                needs_prefetch,
                has_audio_output,
                output_resource_pressure,
            )
        })
    }

    pub(in crate::player::backend::ffmpeg) fn pop_audio_clocked_frame(
        &mut self,
        played_until_nsecs: u64,
    ) -> AudioClockedVideoPopResult {
        let result = pop_audio_clocked_video_frame_with_options(
            &mut self.shared.frames(),
            played_until_nsecs,
            false,
        );
        self.shared.scheduler_dropped_video_frames.fetch_add(
            u64::try_from(result.dropped_frames).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        if result.frame.is_some() || result.dropped_frames > 0 {
            self.shared.changed.notify_all();
        }
        result
    }

    pub(in crate::player::backend::ffmpeg) fn scheduler_dropped_video_frames(&self) -> u64 {
        self.shared
            .scheduler_dropped_video_frames
            .load(Ordering::Relaxed)
    }

    pub(in crate::player::backend::ffmpeg) fn attach_deadline_service(
        &self,
    ) -> ScheduledVideoDeadlineQueue {
        let was_attached = self
            .shared
            .deadline_service_attached
            .swap(true, Ordering::AcqRel);
        assert!(!was_attached, "video deadline service already attached");
        self.shared.changed.notify_all();
        ScheduledVideoDeadlineQueue {
            shared: Arc::clone(&self.shared),
        }
    }

    pub(in crate::player::backend::ffmpeg) fn set_deadline_service_active(&self, active: bool) {
        let _admission = self
            .shared
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = self
            .shared
            .deadline_service_active
            .swap(active, Ordering::AcqRel)
            != active;
        if changed {
            self.shared.advance_presentation_generation();
            self.shared.changed.notify_all();
        }
    }

    pub(in crate::player::backend::ffmpeg) fn deadline_service_owns_presentation(&self) -> bool {
        self.shared
            .deadline_service_attached
            .load(Ordering::Acquire)
            && self.shared.deadline_service_active.load(Ordering::Acquire)
    }

    pub(in crate::player::backend::ffmpeg) fn prebuffer_limit_log_summary(
        &mut self,
        now: Instant,
    ) -> Option<u64> {
        if self
            .last_prebuffer_limit_log_at
            .is_some_and(|last| now.saturating_duration_since(last) < PREBUFFER_LIMIT_LOG_INTERVAL)
        {
            self.suppressed_prebuffer_limit_logs =
                self.suppressed_prebuffer_limit_logs.saturating_add(1);
            return None;
        }
        self.last_prebuffer_limit_log_at = Some(now);
        Some(std::mem::take(&mut self.suppressed_prebuffer_limit_logs))
    }

    pub(in crate::player::backend::ffmpeg) fn record_coordinator_tick(
        &mut self,
        elapsed: Duration,
        completed_at: Instant,
    ) {
        if elapsed < COORDINATOR_STALL_RECORD_THRESHOLD {
            return;
        }
        self.recent_coordinator_stall = Some(CoordinatorStall {
            elapsed,
            completed_at,
        });
    }

    pub(in crate::player::backend::ffmpeg) fn recent_coordinator_stall(
        &self,
        now: Instant,
    ) -> Option<RecentCoordinatorStall> {
        let stall = self.recent_coordinator_stall?;
        let age = now.saturating_duration_since(stall.completed_at);
        (age <= COORDINATOR_STALL_RECENT_WINDOW).then_some(RecentCoordinatorStall {
            elapsed: stall.elapsed,
            age,
        })
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
        pending_audio_pressure_context: &'static str,
        startup_pending_pressure_suppressed_hard_reset: bool,
        log_kind: &'static str,
        suppressed_repeats: u64,
        blocked_for: Duration,
    ) {
        self.with_frames(|frames| {
            log_playback_resume_waterline_wait(
                session_id,
                context,
                playback_output_state,
                resume_timeline_nsecs,
                frames,
                pending_audio,
                waterline,
                demux_watermark,
                pending_audio_pressure_context,
                startup_pending_pressure_suppressed_hard_reset,
                log_kind,
                suppressed_repeats,
                blocked_for,
            );
        });
    }
}

impl ScheduledVideoDeadlineQueue {
    pub(in crate::player::backend::ffmpeg) fn presentation_session_id(&self) -> PlaybackSessionId {
        PlaybackSessionId(self.shared.presentation_session_id.load(Ordering::Acquire))
    }

    pub(in crate::player::backend::ffmpeg) fn active(&self) -> bool {
        self.shared.deadline_service_active.load(Ordering::Acquire)
    }

    pub(in crate::player::backend::ffmpeg) fn suspend_for_session_mismatch(
        &self,
        observed_session_id: PlaybackSessionId,
    ) -> bool {
        let _admission = self
            .shared
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current_session_id = self.presentation_session_id();
        if current_session_id != observed_session_id {
            return false;
        }
        let was_active = self
            .shared
            .deadline_service_active
            .swap(false, Ordering::AcqRel);
        if was_active {
            self.shared.advance_presentation_generation();
            self.shared.changed.notify_all();
        }
        was_active
    }

    pub(in crate::player::backend::ffmpeg) fn wait_for_change(&self, timeout: Duration) {
        let guard = self.shared.frames();
        let _guard = self
            .shared
            .changed
            .wait_timeout(guard, timeout)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0;
    }

    pub(in crate::player::backend::ffmpeg) fn wake(&self) {
        self.shared.changed.notify_all();
    }

    pub(in crate::player::backend::ffmpeg) fn audio_clock_wait_duration(
        &self,
        played_until_nsecs: u64,
    ) -> Option<Duration> {
        audio_clocked_video_wait_duration(&self.shared.frames(), played_until_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn pop_audio_clocked_frame(
        &self,
        played_until_nsecs: u64,
        force_latest_due: bool,
    ) -> (Option<DeadlineVideoFrame>, usize, bool) {
        let _admission = self
            .shared
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.active() {
            return (None, 0, false);
        }
        let generation = self.shared.presentation_generation.load(Ordering::Acquire);
        let session_id =
            PlaybackSessionId(self.shared.presentation_session_id.load(Ordering::Acquire));
        let result = pop_audio_clocked_video_frame_with_options(
            &mut self.shared.frames(),
            played_until_nsecs,
            force_latest_due,
        );
        self.shared.scheduler_dropped_video_frames.fetch_add(
            u64::try_from(result.dropped_frames).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        if result.frame.is_some() || result.dropped_frames > 0 {
            self.shared.changed.notify_all();
        }
        (
            result.frame.map(|frame| DeadlineVideoFrame {
                frame,
                generation,
                session_id,
            }),
            result.dropped_frames,
            result.forced_for_minimum_frame_rate,
        )
    }

    pub(in crate::player::backend::ffmpeg) fn admit_if_current<R>(
        &self,
        pending: DeadlineVideoFrame,
        admit: impl FnOnce(QueuedVideoFrame, PlaybackSessionId) -> R,
    ) -> Option<R> {
        let _admission = self
            .shared
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.active()
            || pending.generation != self.shared.presentation_generation.load(Ordering::Acquire)
            || pending.session_id.0 != self.shared.presentation_session_id.load(Ordering::Acquire)
        {
            return None;
        }
        Some(admit(pending.frame, pending.session_id))
    }

    pub(in crate::player::backend::ffmpeg) fn len(&self) -> usize {
        self.shared.frames().len()
    }

    pub(in crate::player::backend::ffmpeg) fn scheduler_dropped_video_frames(&self) -> u64 {
        self.shared
            .scheduler_dropped_video_frames
            .load(Ordering::Relaxed)
    }

    pub(in crate::player::backend::ffmpeg) fn detach(&self) {
        let _admission = self
            .shared
            .admission_gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.shared
            .deadline_service_active
            .store(false, Ordering::Release);
        self.shared
            .deadline_service_attached
            .store(false, Ordering::Release);
        self.shared.advance_presentation_generation();
        self.shared.changed.notify_all();
    }
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn pop_audio_clocked_video_frame(
    queued_video_frames: &mut VecDeque<QueuedVideoFrame>,
    played_until_nsecs: u64,
) -> Option<QueuedVideoFrame> {
    pop_audio_clocked_video_frame_with_policy(queued_video_frames, played_until_nsecs).frame
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn pop_audio_clocked_video_frame_with_policy(
    queued_video_frames: &mut VecDeque<QueuedVideoFrame>,
    played_until_nsecs: u64,
) -> AudioClockedVideoPopResult {
    pop_audio_clocked_video_frame_with_options(queued_video_frames, played_until_nsecs, false)
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn pop_audio_clocked_video_frame_with_minimum_rate(
    queued_video_frames: &mut VecDeque<QueuedVideoFrame>,
    played_until_nsecs: u64,
) -> AudioClockedVideoPopResult {
    pop_audio_clocked_video_frame_with_options(queued_video_frames, played_until_nsecs, true)
}

fn pop_audio_clocked_video_frame_with_options(
    queued_video_frames: &mut VecDeque<QueuedVideoFrame>,
    played_until_nsecs: u64,
    force_latest_due: bool,
) -> AudioClockedVideoPopResult {
    let mut result = AudioClockedVideoPopResult::default();
    if force_latest_due {
        let mut due_frame = None;
        while queued_video_frames
            .front()
            .is_some_and(|frame| frame.timeline_nsecs <= played_until_nsecs)
        {
            if due_frame.is_some() {
                result.dropped_frames = result.dropped_frames.saturating_add(1);
            }
            due_frame = queued_video_frames.pop_front();
        }
        if due_frame.is_none()
            && queued_video_frame_ready_for_audio_clock(queued_video_frames, played_until_nsecs)
        {
            due_frame = queued_video_frames.pop_front();
        }
        result.forced_for_minimum_frame_rate = due_frame.is_some();
        result.frame = due_frame;
        return result;
    }

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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use crate::player::render_host::{DecodedFrame, FramePixels, FramePts, RenderSize};

    use super::*;

    fn queued_video_frame(timeline_nsecs: u64, duration_nsecs: u64) -> QueuedVideoFrame {
        QueuedVideoFrame {
            frame: DecodedFrame {
                size: RenderSize {
                    width: 1,
                    height: 1,
                },
                pts: Some(FramePts {
                    nsecs: timeline_nsecs,
                }),
                key_frame: false,
                pixels: FramePixels::Bgra8(vec![0, 0, 0, 255].into()),
            },
            timeline_nsecs,
            duration_nsecs,
            source_duration_nsecs: duration_nsecs,
        }
    }

    #[test]
    fn contiguous_video_forward_stops_before_large_pts_gap() {
        let mut queue = VecDeque::new();
        queue.push_back(queued_video_frame(252_760_000_000, 40_000_000));
        queue.push_back(queued_video_frame(252_800_000_000, 40_000_000));
        queue.push_back(queued_video_frame(252_840_000_000, 40_000_000));
        queue.push_back(queued_video_frame(252_880_000_000, 40_000_000));
        queue.push_back(queued_video_frame(257_720_000_000, 40_000_000));

        assert_eq!(
            queued_video_buffered_until_nsecs(&queue),
            Some(257_760_000_000)
        );
        assert_eq!(
            queued_video_buffered_until_from_nsecs(&queue, 252_880_000_000),
            Some(252_920_000_000)
        );
        assert_eq!(
            queued_video_forward_nsecs_from(&queue, 252_880_000_000),
            Some(40_000_000)
        );
        assert_eq!(queued_video_largest_gap_nsecs(&queue), Some(4_800_000_000));
    }

    #[test]
    fn initial_lookahead_counts_only_the_contiguous_head() {
        let mut queue = ScheduledVideoQueue::default();
        queue.push_queued(queued_video_frame(1_000_000_000, 40_000_000));
        queue.push_queued(queued_video_frame(2_000_000_000, 40_000_000));
        assert_eq!(queue.contiguous_frame_count_from(1_000_000_000), 1);

        queue.push_queued(queued_video_frame(1_040_000_000, 40_000_000));
        assert_eq!(queue.contiguous_frame_count_from(1_000_000_000), 2);
    }

    #[test]
    fn contiguous_video_forward_allows_small_timestamp_jitter() {
        let mut queue = VecDeque::new();
        queue.push_back(queued_video_frame(1_000_000_000, 40_000_000));
        queue.push_back(queued_video_frame(1_080_000_000, 40_000_000));
        queue.push_back(queued_video_frame(1_120_000_000, 40_000_000));

        assert_eq!(
            queued_video_buffered_until_from_nsecs(&queue, 1_000_000_000),
            Some(1_160_000_000)
        );
        assert_eq!(
            queued_video_forward_nsecs_from(&queue, 1_000_000_000),
            Some(160_000_000)
        );
    }

    #[test]
    fn explicit_contiguous_video_gap_threshold_is_honored() {
        let mut queue = VecDeque::new();
        queue.push_back(queued_video_frame(1_000_000_000, 40_000_000));
        queue.push_back(queued_video_frame(1_190_000_000, 40_000_000));

        assert_eq!(
            queued_video_contiguous_buffered_until_from_nsecs(&queue, 1_000_000_000, 100_000_000),
            Some(1_040_000_000)
        );
        assert_eq!(
            queued_video_contiguous_buffered_until_from_nsecs(&queue, 1_000_000_000, 200_000_000),
            Some(1_230_000_000)
        );
    }

    #[test]
    fn continuity_threshold_tolerates_nanosecond_timestamp_rounding() {
        let previous_expected_next_nsecs = 174_116_666_666_u64;
        let next_timeline_nsecs = 174_616_666_667_u64;
        let gap_nsecs = next_timeline_nsecs.saturating_sub(previous_expected_next_nsecs);

        assert_eq!(gap_nsecs, 500_000_001);
        assert!(video_timestamp_gap_within_threshold(
            gap_nsecs,
            VIDEO_CONTIGUITY_MAX_GAP_NSECS,
        ));
        assert!(!video_timestamp_gap_within_threshold(
            VIDEO_CONTIGUITY_MAX_GAP_NSECS + VIDEO_TIMESTAMP_ROUNDING_TOLERANCE_NSECS + 1,
            VIDEO_CONTIGUITY_MAX_GAP_NSECS,
        ));
    }

    #[test]
    fn explicit_continuity_scan_applies_timestamp_rounding_tolerance() {
        let mut queue = VecDeque::new();
        queue.push_back(queued_video_frame(1_000_000_000, 40_000_000));
        queue.push_back(queued_video_frame(1_240_000_001, 40_000_000));

        assert_eq!(
            queued_video_contiguous_buffered_until_from_nsecs(&queue, 1_000_000_000, 200_000_000),
            Some(1_280_000_001)
        );
    }

    #[test]
    fn strict_startup_lookahead_tolerates_mux_timestamp_quantization() {
        let mut queue = ScheduledVideoQueue::default();
        let first_nsecs = 535_833_312_500;
        let frame_duration_nsecs = 33_333_332;
        for timeline_nsecs in [
            first_nsecs,
            535_866_687_500,
            535_900_000_000,
            535_933_312_500,
        ] {
            queue.push_queued(queued_video_frame(timeline_nsecs, frame_duration_nsecs));
        }

        assert!(
            queue
                .strict_forward_nsecs_from(first_nsecs)
                .is_some_and(|forward_nsecs| forward_nsecs >= 80_000_000)
        );
    }

    #[test]
    fn strict_startup_lookahead_stops_at_real_timestamp_gap() {
        let mut queue = ScheduledVideoQueue::default();
        let first_nsecs = 1_000_000_000;
        let frame_duration_nsecs = 40_000_000;
        queue.push_queued(queued_video_frame(first_nsecs, frame_duration_nsecs));
        queue.push_queued(queued_video_frame(
            first_nsecs
                + frame_duration_nsecs
                + VIDEO_STRICT_CONTINUITY_MAX_TIMESTAMP_JITTER_NSECS
                + VIDEO_TIMESTAMP_ROUNDING_TOLERANCE_NSECS
                + 1,
            frame_duration_nsecs,
        ));

        assert_eq!(
            queue.strict_forward_nsecs_from(first_nsecs),
            Some(frame_duration_nsecs)
        );
    }

    #[test]
    fn extending_tail_duration_bridges_confirmed_media_timeline_gap() {
        let mut queue = ScheduledVideoQueue::default();
        queue.push_queued(queued_video_frame(1_000_000_000, 40_000_000));

        assert_eq!(
            queue.extend_back_duration_to(1_834_000_000),
            Some((40_000_000, 834_000_000))
        );
        queue.push_queued(queued_video_frame(1_834_000_000, 40_000_000));

        assert_eq!(
            queue.buffered_until_from_nsecs(1_000_000_000),
            Some(1_874_000_000)
        );
        assert_eq!(queue.largest_gap_nsecs(), None);
    }

    #[test]
    fn accepted_hold_gap_does_not_fake_decoded_queue_backpressure() {
        let mut queue = ScheduledVideoQueue::default();
        let last_good_nsecs = 90_533_312_500;
        let next_idr_nsecs = 95_000_000_000;
        let frame_duration_nsecs = 33_333_333;
        queue.push_queued(queued_video_frame(last_good_nsecs, frame_duration_nsecs));

        queue
            .extend_back_duration_to(next_idr_nsecs)
            .expect("the accepted HEVC gap extends the last good frame");
        queue.push_queued(queued_video_frame(next_idr_nsecs, frame_duration_nsecs));

        assert!(
            queue.duration() > queue.limit_duration(false),
            "the display timeline must still hold the last good frame until the IDR"
        );
        assert!(
            !queue.limit_reached(false),
            "the held interval is not decoded output and must not stop lookahead"
        );
    }

    #[test]
    fn scheduler_drop_counter_is_distinct_and_keeps_recent_stall_metadata() {
        let mut queue = ScheduledVideoQueue::default();
        queue.push_queued(queued_video_frame(0, 40_000_000));
        queue.push_queued(queued_video_frame(40_000_000, 40_000_000));
        queue.push_queued(queued_video_frame(80_000_000, 40_000_000));
        let now = Instant::now();
        queue.record_coordinator_tick(Duration::from_millis(65), now);

        let result = queue.pop_audio_clocked_frame(500_000_000);
        assert_eq!(result.dropped_frames, 3);
        assert_eq!(queue.scheduler_dropped_video_frames(), 3);
        let stall = queue
            .recent_coordinator_stall(now + Duration::from_millis(5))
            .expect("65 ms coordinator delay remains recent");
        assert_eq!(stall.elapsed, Duration::from_millis(65));
        assert_eq!(stall.age, Duration::from_millis(5));

        queue.clear();
        assert_eq!(queue.scheduler_dropped_video_frames(), 0);
        assert!(queue.recent_coordinator_stall(Instant::now()).is_none());
    }

    #[test]
    fn prebuffer_limit_log_is_summarized_every_half_second() {
        let mut queue = ScheduledVideoQueue::default();
        let now = Instant::now();

        assert_eq!(queue.prebuffer_limit_log_summary(now), Some(0));
        assert_eq!(
            queue.prebuffer_limit_log_summary(now + Duration::from_millis(100)),
            None
        );
        assert_eq!(
            queue.prebuffer_limit_log_summary(now + Duration::from_millis(499)),
            None
        );
        assert_eq!(
            queue.prebuffer_limit_log_summary(now + Duration::from_millis(500)),
            Some(2)
        );
    }

    #[test]
    fn minimum_frame_rate_policy_keeps_latest_frame_when_all_due_frames_are_stale() {
        let mut queue = VecDeque::from([
            queued_video_frame(1_000_000_000, 16_000_000),
            queued_video_frame(1_020_000_000, 16_000_000),
            queued_video_frame(1_040_000_000, 16_000_000),
        ]);

        let result = pop_audio_clocked_video_frame_with_minimum_rate(&mut queue, 1_500_000_000);

        assert_eq!(
            result
                .frame
                .expect("latest stale frame is retained")
                .timeline_nsecs,
            1_040_000_000
        );
        assert_eq!(result.dropped_frames, 2);
        assert!(result.forced_for_minimum_frame_rate);
        assert!(queue.is_empty());
    }

    #[test]
    fn ordinary_late_policy_may_drop_every_stale_frame() {
        let mut queue = VecDeque::from([
            queued_video_frame(1_000_000_000, 16_000_000),
            queued_video_frame(1_020_000_000, 16_000_000),
        ]);

        let result = pop_audio_clocked_video_frame_with_policy(&mut queue, 1_500_000_000);

        assert!(result.frame.is_none());
        assert_eq!(result.dropped_frames, 2);
        assert!(!result.forced_for_minimum_frame_rate);
    }

    #[test]
    fn deadline_queue_generation_fences_frame_popped_before_rebuffer() {
        let mut queue = ScheduledVideoQueue::default();
        let deadline = queue.attach_deadline_service();
        queue.set_deadline_service_active(true);
        queue.push_queued(queued_video_frame(1_000_000_000, 40_000_000));
        let (pending, _, _) = deadline.pop_audio_clocked_frame(1_000_000_000, false);
        let pending = pending.expect("deadline service pops due frame");

        queue.set_deadline_service_active(false);

        assert_eq!(deadline.admit_if_current(pending, |_, _| true), None);
        assert!(!queue.deadline_service_owns_presentation());
        deadline.detach();
    }

    #[test]
    fn deadline_queue_rebinds_frames_to_the_seek_session() {
        let mut queue = ScheduledVideoQueue::default();
        let first_session = PlaybackSessionId(17);
        let seek_session = PlaybackSessionId(18);
        queue.clear_and_bind_presentation_session(first_session);
        let deadline = queue.attach_deadline_service();
        queue.set_deadline_service_active(true);
        queue.push_queued(queued_video_frame(1_000_000_000, 40_000_000));
        let (stale, _, _) = deadline.pop_audio_clocked_frame(1_000_000_000, false);
        let stale = stale.expect("deadline service pops the pre-seek frame");

        queue.set_deadline_service_active(false);
        queue.clear_and_bind_presentation_session(seek_session);
        queue.set_deadline_service_active(true);

        assert_eq!(
            deadline.admit_if_current(stale, |_, session_id| session_id),
            None,
            "a popped pre-seek frame must not be relabelled as the new session"
        );

        queue.push_queued(queued_video_frame(2_000_000_000, 40_000_000));
        let (current, _, _) = deadline.pop_audio_clocked_frame(2_000_000_000, false);
        let current = current.expect("deadline service pops the post-seek frame");
        assert_eq!(
            deadline.admit_if_current(current, |_, session_id| session_id),
            Some(seek_session)
        );
        deadline.detach();
    }

    #[test]
    fn deadline_session_mismatch_suspends_without_draining_frames() {
        let mut queue = ScheduledVideoQueue::default();
        let session_id = PlaybackSessionId(21);
        queue.clear_and_bind_presentation_session(session_id);
        let deadline = queue.attach_deadline_service();
        queue.set_deadline_service_active(true);
        queue.push_queued(queued_video_frame(1_000_000_000, 40_000_000));

        assert!(deadline.suspend_for_session_mismatch(session_id));
        assert!(!queue.deadline_service_owns_presentation());
        assert_eq!(queue.len(), 1);
        assert!(!deadline.suspend_for_session_mismatch(session_id));
        deadline.detach();
    }
}
