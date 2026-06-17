use super::*;

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WaitStatus {
    Ready,
    Interrupted,
}

pub(super) struct QueuedVideoFrame {
    pub(super) frame: DecodedFrame,
    pub(super) timeline_nsecs: u64,
    pub(super) duration_nsecs: u64,
}

pub(super) fn queued_video_duration(queued_video_frames: &VecDeque<QueuedVideoFrame>) -> Duration {
    match (queued_video_frames.front(), queued_video_frames.back()) {
        (Some(first), Some(last)) => {
            Duration::from_nanos(last.timeline_nsecs.saturating_sub(first.timeline_nsecs))
        }
        _ => Duration::ZERO,
    }
}

pub(super) fn queued_video_limit_duration(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    needs_subtitle_prefetch: bool,
) -> Duration {
    if queued_video_frames_have_vulkan(queued_video_frames) {
        VULKAN_AUDIO_VIDEO_QUEUE_LIMIT_DURATION
    } else if needs_subtitle_prefetch {
        PGS_SUBTITLE_VIDEO_QUEUE_LIMIT_DURATION
    } else {
        AUDIO_VIDEO_QUEUE_LIMIT_DURATION
    }
}

pub(super) fn queued_video_target_duration(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    needs_subtitle_prefetch: bool,
) -> Duration {
    if queued_video_frames_have_vulkan(queued_video_frames) {
        VULKAN_AUDIO_VIDEO_QUEUE_TARGET_DURATION
    } else if needs_subtitle_prefetch {
        PGS_SUBTITLE_VIDEO_QUEUE_TARGET_DURATION
    } else {
        AUDIO_VIDEO_QUEUE_TARGET_DURATION
    }
}

pub(super) fn queued_video_limit_frames(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    _needs_subtitle_prefetch: bool,
) -> usize {
    if queued_video_frames_have_vulkan(queued_video_frames) {
        return VULKAN_DECODED_VIDEO_QUEUE_LIMIT_FRAMES;
    }
    DECODED_VIDEO_QUEUE_LIMIT_FRAMES
}

pub(super) fn queued_video_target_frames(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    _needs_subtitle_prefetch: bool,
) -> usize {
    if queued_video_frames_have_vulkan(queued_video_frames) {
        return VULKAN_DECODED_VIDEO_QUEUE_TARGET_FRAMES;
    }
    DECODED_VIDEO_QUEUE_TARGET_FRAMES
}

pub(super) fn queued_video_limit_reached(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    needs_subtitle_prefetch: bool,
) -> bool {
    !queued_video_frames.is_empty()
        && (queued_video_duration(queued_video_frames)
            >= queued_video_limit_duration(queued_video_frames, needs_subtitle_prefetch)
            || queued_video_frames.len()
                >= queued_video_limit_frames(queued_video_frames, needs_subtitle_prefetch))
}

#[cfg(test)]
pub(super) fn queued_video_target_reached(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
    needs_subtitle_prefetch: bool,
) -> bool {
    !queued_video_frames.is_empty()
        && (queued_video_duration(queued_video_frames)
            >= queued_video_target_duration(queued_video_frames, needs_subtitle_prefetch)
            || queued_video_frames.len()
                >= queued_video_target_frames(queued_video_frames, needs_subtitle_prefetch))
}

pub(super) fn queued_video_frames_have_vulkan(
    queued_video_frames: &VecDeque<QueuedVideoFrame>,
) -> bool {
    queued_video_frames
        .iter()
        .any(|frame| matches!(&frame.frame.pixels, FramePixels::VulkanVideo(_)))
}

pub(super) fn frame_decode_error_flags(frame: *mut ffi::AVFrame) -> c_int {
    if frame.is_null() {
        return 0;
    }

    unsafe { (*frame).decode_error_flags }
}

pub(super) fn frame_is_corrupt(frame: *mut ffi::AVFrame) -> bool {
    if frame.is_null() {
        return false;
    }

    unsafe {
        ((*frame).flags & ffi::AV_FRAME_FLAG_CORRUPT) != 0 || (*frame).decode_error_flags != 0
    }
}

pub(super) struct PlaybackScheduler {
    start_instant: Instant,
    start_position_nsecs: u64,
}

impl PlaybackScheduler {
    pub(super) fn new(start_position_nsecs: u64) -> Self {
        Self {
            start_instant: Instant::now(),
            start_position_nsecs,
        }
    }

    pub(super) fn reset(&mut self, start_position_nsecs: u64) {
        self.start_instant = Instant::now();
        self.start_position_nsecs = start_position_nsecs;
    }

    pub(super) fn delay_by(&mut self, duration: Duration) {
        self.start_instant = self
            .start_instant
            .checked_add(duration)
            .unwrap_or(self.start_instant);
    }

    pub(super) fn ready_for(&self, timeline_nsecs: u64) -> bool {
        let target_offset = timeline_nsecs.saturating_sub(self.start_position_nsecs);
        let target = self
            .start_instant
            .checked_add(Duration::from_nanos(target_offset))
            .unwrap_or(self.start_instant);
        Instant::now() >= target
    }

    #[cfg(test)]
    pub(super) fn wait_until(
        &mut self,
        timeline_nsecs: u64,
        control: &FfmpegControl,
    ) -> WaitStatus {
        let target_offset = timeline_nsecs.saturating_sub(self.start_position_nsecs);
        loop {
            if control.should_interrupt() {
                return WaitStatus::Interrupted;
            }
            if control.is_paused() {
                let paused_at = Instant::now();
                if control.wait_while_paused() || control.has_pending_seek() {
                    return WaitStatus::Interrupted;
                }
                let paused_for = paused_at.elapsed();
                self.start_instant = self
                    .start_instant
                    .checked_add(paused_for)
                    .unwrap_or(self.start_instant);
                continue;
            }

            let target = self
                .start_instant
                .checked_add(Duration::from_nanos(target_offset))
                .unwrap_or(self.start_instant);
            let now = Instant::now();
            if now >= target {
                return WaitStatus::Ready;
            }
            thread::sleep((target - now).min(SCHEDULER_POLL_INTERVAL));
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct MappedTimestamp {
    pub(super) timeline_nsecs: u64,
    pub(super) sink_nsecs: u64,
}

pub(super) struct TimestampMapper {
    start_nsecs: Option<u64>,
    fallback_first_nsecs: Option<u64>,
    start_position_nsecs: u64,
    fallback_step_nsecs: u64,
    last_timeline_nsecs: Option<u64>,
    last_contiguous_end_nsecs: Option<u64>,
}

impl TimestampMapper {
    pub(super) fn new(
        start_nsecs: Option<u64>,
        start_position_nsecs: u64,
        fallback_step_nsecs: Option<u64>,
    ) -> Self {
        Self {
            start_nsecs,
            fallback_first_nsecs: None,
            start_position_nsecs,
            fallback_step_nsecs: fallback_step_nsecs.unwrap_or(1),
            last_timeline_nsecs: None,
            last_contiguous_end_nsecs: None,
        }
    }

    pub(super) fn map(&mut self, timestamp: i64, time_base: ffi::AVRational) -> MappedTimestamp {
        let mut timeline_nsecs = timestamp_to_nsecs(timestamp, time_base)
            .map(|nsecs| self.timeline_from_timestamp(nsecs))
            .unwrap_or_else(|| self.next_synthetic_timeline());

        if self.start_position_nsecs > 0 && timeline_nsecs == 0 {
            timeline_nsecs = self.next_synthetic_timeline();
        }
        if let Some(last_timeline_nsecs) = self.last_timeline_nsecs
            && timeline_nsecs <= last_timeline_nsecs
        {
            timeline_nsecs = last_timeline_nsecs.saturating_add(self.fallback_step_nsecs);
        }

        self.last_timeline_nsecs = Some(timeline_nsecs);
        MappedTimestamp {
            timeline_nsecs,
            sink_nsecs: timeline_nsecs.saturating_sub(self.start_position_nsecs),
        }
    }

    pub(super) fn map_contiguous(
        &mut self,
        timestamp: i64,
        time_base: ffi::AVRational,
        frame_duration_nsecs: u64,
        continuity_tolerance: Duration,
    ) -> MappedTimestamp {
        let previous_end_nsecs = self.last_contiguous_end_nsecs;
        let mut mapped = self.map(timestamp, time_base);
        if let Some(previous_end_nsecs) = previous_end_nsecs {
            let drift_nsecs = mapped.timeline_nsecs.abs_diff(previous_end_nsecs);
            if drift_nsecs <= duration_nsecs(continuity_tolerance) {
                mapped.timeline_nsecs = previous_end_nsecs;
                mapped.sink_nsecs = mapped
                    .timeline_nsecs
                    .saturating_sub(self.start_position_nsecs);
                self.last_timeline_nsecs = Some(mapped.timeline_nsecs);
            }
        }
        self.last_contiguous_end_nsecs =
            Some(mapped.timeline_nsecs.saturating_add(frame_duration_nsecs));
        mapped
    }

    pub(super) fn timeline_origin_nsecs(&self) -> Option<u64> {
        self.start_nsecs.or_else(|| {
            self.fallback_first_nsecs
                .map(|first| first.saturating_sub(self.start_position_nsecs))
        })
    }

    fn timeline_from_timestamp(&mut self, nsecs: u64) -> u64 {
        if let Some(start_nsecs) = self.start_nsecs {
            nsecs.saturating_sub(start_nsecs)
        } else {
            let first_nsecs = *self.fallback_first_nsecs.get_or_insert(nsecs);
            self.start_position_nsecs
                .saturating_add(nsecs.saturating_sub(first_nsecs))
        }
    }

    fn next_synthetic_timeline(&self) -> u64 {
        self.last_timeline_nsecs
            .map(|last| last.saturating_add(self.fallback_step_nsecs))
            .unwrap_or(self.start_position_nsecs)
    }
}

pub(super) fn frame_best_effort_timestamp(frame: *mut ffi::AVFrame) -> i64 {
    unsafe {
        if (*frame).best_effort_timestamp != ffi::AV_NOPTS_VALUE {
            (*frame).best_effort_timestamp
        } else {
            (*frame).pts
        }
    }
}

pub(super) fn timestamp_to_nsecs(timestamp: i64, time_base: ffi::AVRational) -> Option<u64> {
    if timestamp == ffi::AV_NOPTS_VALUE || time_base.den <= 0 {
        return None;
    }
    let nsecs_time_base = ffi::AVRational {
        num: 1,
        den: 1_000_000_000,
    };
    let nsecs = unsafe { ffi::av_rescale_q(timestamp, time_base, nsecs_time_base) };
    u64::try_from(nsecs).ok()
}

pub(super) unsafe fn stream_frame_duration_nsecs(stream: *mut ffi::AVStream) -> Option<u64> {
    if stream.is_null() {
        return None;
    }

    unsafe {
        rational_frame_duration_nsecs((*stream).avg_frame_rate)
            .or_else(|| rational_frame_duration_nsecs((*stream).r_frame_rate))
    }
}

pub(super) fn rational_frame_duration_nsecs(rate: ffi::AVRational) -> Option<u64> {
    if rate.num <= 0 || rate.den <= 0 {
        return None;
    }

    Some(((rate.den as u64).saturating_mul(1_000_000_000) / rate.num as u64).max(1))
}

pub(super) fn seconds_to_nsecs(seconds: f64) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0;
    }

    (seconds * 1_000_000_000.0).round().min(u64::MAX as f64) as u64
}

pub(super) fn nsecs_to_timestamp(nsecs: u64, time_base: ffi::AVRational) -> i64 {
    let nsecs_time_base = ffi::AVRational {
        num: 1,
        den: 1_000_000_000,
    };
    let nsecs = i64::try_from(nsecs).unwrap_or(i64::MAX);
    unsafe { ffi::av_rescale_q(nsecs, nsecs_time_base, time_base) }
}

pub(super) fn nsecs_to_seconds(nsecs: u64) -> f64 {
    nsecs as f64 / 1_000_000_000.0
}

pub(super) fn max_optional_seconds(current: Option<f64>, timeline_nsecs: u64) -> f64 {
    let next = nsecs_to_seconds(timeline_nsecs);
    current.map(|current| current.max(next)).unwrap_or(next)
}

pub(super) fn optional_buffered_value_changed(previous: Option<f64>, next: Option<f64>) -> bool {
    match (previous, next) {
        (None, None) => false,
        (Some(previous), Some(next)) => (previous - next).abs() >= 0.05,
        _ => true,
    }
}

pub(super) fn duration_nsecs(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

pub(super) fn pts_distance(left: FramePts, right: FramePts) -> u64 {
    left.nsecs.abs_diff(right.nsecs)
}
