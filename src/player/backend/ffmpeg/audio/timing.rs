use super::{
    AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER, AudioOutputSnapshot, AudioQueueSnapshot,
    AudioSharedSnapshot, Duration, c_int,
};

pub(in crate::player::backend::ffmpeg::audio) struct AudioOutputTryPushTimedTiming {
    pub(in crate::player::backend::ffmpeg::audio) result: &'static str,
    pub(in crate::player::backend::ffmpeg::audio) total: Duration,
    pub(in crate::player::backend::ffmpeg::audio) queue_lock_wait: Duration,
    pub(in crate::player::backend::ffmpeg::audio) sample_count: usize,
    pub(in crate::player::backend::ffmpeg::audio) misaligned_audio_buffer_count: u64,
    pub(in crate::player::backend::ffmpeg::audio) start_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) end_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) queued_frames: usize,
    pub(in crate::player::backend::ffmpeg::audio) queued_duration: Duration,
}

pub(in crate::player::backend::ffmpeg::audio) struct AudioOutputSnapshotTiming {
    pub(in crate::player::backend::ffmpeg::audio) total: Duration,
    pub(in crate::player::backend::ffmpeg::audio) shared_snapshot: Duration,
    pub(in crate::player::backend::ffmpeg::audio) queue_snapshot: Duration,
    pub(in crate::player::backend::ffmpeg::audio) underrun_recheck: Duration,
    pub(in crate::player::backend::ffmpeg::audio) misaligned_audio_buffer_count: u64,
    pub(in crate::player::backend::ffmpeg::audio) snapshot: AudioOutputSnapshot,
}

pub(in crate::player::backend::ffmpeg::audio) fn log_audio_output_try_push_timed_timing(
    timing: AudioOutputTryPushTimedTiming,
) {
    tracing::trace!(
        result = timing.result,
        total_ms = timing.total.as_secs_f64() * 1000.0,
        queue_lock_wait_ms = timing.queue_lock_wait.as_secs_f64() * 1000.0,
        sample_count = timing.sample_count,
        misaligned_audio_buffer_count = timing.misaligned_audio_buffer_count,
        start_timeline_nsecs = timing.start_timeline_nsecs,
        end_timeline_nsecs = timing.end_timeline_nsecs,
        queued_frames = timing.queued_frames,
        queued_ms = timing.queued_duration.as_secs_f64() * 1000.0,
        "native audio output try_push_timed timing"
    );
    if timing.total < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
        && timing.queue_lock_wait < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        result = timing.result,
        total_ms = timing.total.as_secs_f64() * 1000.0,
        queue_lock_wait_ms = timing.queue_lock_wait.as_secs_f64() * 1000.0,
        sample_count = timing.sample_count,
        misaligned_audio_buffer_count = timing.misaligned_audio_buffer_count,
        start_timeline_nsecs = timing.start_timeline_nsecs,
        end_timeline_nsecs = timing.end_timeline_nsecs,
        queued_frames = timing.queued_frames,
        queued_ms = timing.queued_duration.as_secs_f64() * 1000.0,
        "native audio output try_push_timed completed slowly"
    );
}

pub(in crate::player::backend::ffmpeg::audio) fn log_audio_output_reset_clock_timing(
    timeline_nsecs: u64,
    total: Duration,
    queue_clear: Duration,
    shared_reset: Duration,
) {
    tracing::trace!(
        timeline_nsecs,
        total_ms = total.as_secs_f64() * 1000.0,
        queue_clear_ms = queue_clear.as_secs_f64() * 1000.0,
        shared_reset_ms = shared_reset.as_secs_f64() * 1000.0,
        "native audio output reset_clock timing"
    );
    if total < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
        && queue_clear < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
        && shared_reset < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        timeline_nsecs,
        total_ms = total.as_secs_f64() * 1000.0,
        queue_clear_ms = queue_clear.as_secs_f64() * 1000.0,
        shared_reset_ms = shared_reset.as_secs_f64() * 1000.0,
        "native audio output reset_clock completed slowly"
    );
}

pub(in crate::player::backend::ffmpeg::audio) fn log_audio_output_snapshot_timing(
    timing: AudioOutputSnapshotTiming,
) {
    tracing::trace!(
        total_ms = timing.total.as_secs_f64() * 1000.0,
        shared_snapshot_ms = timing.shared_snapshot.as_secs_f64() * 1000.0,
        queue_snapshot_ms = timing.queue_snapshot.as_secs_f64() * 1000.0,
        underrun_recheck_ms = timing.underrun_recheck.as_secs_f64() * 1000.0,
        misaligned_audio_buffer_count = timing.misaligned_audio_buffer_count,
        played_timeline_nsecs = timing.snapshot.played_timeline_nsecs,
        pending_ms = timing.snapshot.total_pending_nsecs as f64 / 1_000_000.0,
        shared_pending_ms = timing.snapshot.shared_pending_nsecs as f64 / 1_000_000.0,
        queue_pending_ms = timing.snapshot.queue_pending_nsecs as f64 / 1_000_000.0,
        queue_frames = timing.snapshot.queue_frames,
        queue_generation = timing.snapshot.queue_generation,
        "native audio output snapshot timing"
    );
    if timing.total < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
        && timing.shared_snapshot < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
        && timing.queue_snapshot < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
        && timing.underrun_recheck < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        total_ms = timing.total.as_secs_f64() * 1000.0,
        shared_snapshot_ms = timing.shared_snapshot.as_secs_f64() * 1000.0,
        queue_snapshot_ms = timing.queue_snapshot.as_secs_f64() * 1000.0,
        underrun_recheck_ms = timing.underrun_recheck.as_secs_f64() * 1000.0,
        misaligned_audio_buffer_count = timing.misaligned_audio_buffer_count,
        played_timeline_nsecs = timing.snapshot.played_timeline_nsecs,
        pending_ms = timing.snapshot.total_pending_nsecs as f64 / 1_000_000.0,
        shared_pending_ms = timing.snapshot.shared_pending_nsecs as f64 / 1_000_000.0,
        queue_pending_ms = timing.snapshot.queue_pending_nsecs as f64 / 1_000_000.0,
        queue_frames = timing.snapshot.queue_frames,
        queue_generation = timing.snapshot.queue_generation,
        "native audio output snapshot completed slowly"
    );
}

pub(in crate::player::backend::ffmpeg::audio) fn log_audio_shared_snapshot_timing(
    total: Duration,
    buffer_lock_wait: Duration,
    queued_samples: usize,
    snapshot: AudioSharedSnapshot,
) {
    tracing::trace!(
        total_ms = total.as_secs_f64() * 1000.0,
        buffer_lock_wait_ms = buffer_lock_wait.as_secs_f64() * 1000.0,
        queued_samples,
        played_timeline_nsecs = snapshot.played_timeline_nsecs,
        pending_ms = snapshot.pending_nsecs as f64 / 1_000_000.0,
        "native audio shared snapshot timing"
    );
    if total < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
        && buffer_lock_wait < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        total_ms = total.as_secs_f64() * 1000.0,
        buffer_lock_wait_ms = buffer_lock_wait.as_secs_f64() * 1000.0,
        queued_samples,
        played_timeline_nsecs = snapshot.played_timeline_nsecs,
        pending_ms = snapshot.pending_nsecs as f64 / 1_000_000.0,
        "native audio shared snapshot completed slowly"
    );
}

pub(in crate::player::backend::ffmpeg::audio) fn log_audio_queue_snapshot_timing(
    total: Duration,
    lock_wait: Duration,
    snapshot: AudioQueueSnapshot,
) {
    tracing::trace!(
        total_ms = total.as_secs_f64() * 1000.0,
        lock_wait_ms = lock_wait.as_secs_f64() * 1000.0,
        pending_ms = snapshot.pending_nsecs as f64 / 1_000_000.0,
        frames = snapshot.frames,
        generation = snapshot.generation,
        "native audio queue snapshot timing"
    );
    if total < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
        && lock_wait < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        total_ms = total.as_secs_f64() * 1000.0,
        lock_wait_ms = lock_wait.as_secs_f64() * 1000.0,
        pending_ms = snapshot.pending_nsecs as f64 / 1_000_000.0,
        frames = snapshot.frames,
        generation = snapshot.generation,
        "native audio queue snapshot completed slowly"
    );
}

pub(in crate::player::backend::ffmpeg::audio) fn log_audio_shared_reset_clock_timing(
    timeline_nsecs: u64,
    total: Duration,
    buffer_lock_wait: Duration,
    buffer_cleared: bool,
) {
    tracing::trace!(
        timeline_nsecs,
        total_ms = total.as_secs_f64() * 1000.0,
        buffer_lock_wait_ms = buffer_lock_wait.as_secs_f64() * 1000.0,
        buffer_cleared,
        "native audio shared reset_clock timing"
    );
    if total < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
        && buffer_lock_wait < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        timeline_nsecs,
        total_ms = total.as_secs_f64() * 1000.0,
        buffer_lock_wait_ms = buffer_lock_wait.as_secs_f64() * 1000.0,
        buffer_cleared,
        "native audio shared reset_clock completed slowly"
    );
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn audio_samples_duration(
    samples: usize,
    sample_rate: c_int,
    channels: c_int,
) -> Duration {
    audio_elements_duration(samples, sample_rate, channels)
}

pub(in crate::player::backend::ffmpeg) fn audio_elements_duration(
    elements: usize,
    sample_rate: c_int,
    channels: c_int,
) -> Duration {
    duration_for_audio_frames(audio_frames_for_elements(elements, channels), sample_rate)
}

pub(in crate::player::backend::ffmpeg) fn duration_for_audio_frames(
    frames: u64,
    sample_rate: c_int,
) -> Duration {
    if frames == 0 || sample_rate <= 0 {
        return Duration::ZERO;
    }

    let nanos = (frames as u128).saturating_mul(1_000_000_000) / sample_rate as u128;
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

pub(in crate::player::backend::ffmpeg::audio) fn interpolated_audio_timeline_nsecs(
    start_timeline_nsecs: u64,
    end_timeline_nsecs: u64,
    written_samples: usize,
    total_samples: usize,
) -> u64 {
    if written_samples >= total_samples || total_samples == 0 {
        return end_timeline_nsecs;
    }

    let duration = end_timeline_nsecs.saturating_sub(start_timeline_nsecs);
    let written_duration =
        (duration as u128).saturating_mul(written_samples as u128) / total_samples as u128;
    start_timeline_nsecs.saturating_add(u64::try_from(written_duration).unwrap_or(u64::MAX))
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn samples_for_duration(
    timeline_nsecs: u64,
    sample_rate: c_int,
    channels: c_int,
) -> u64 {
    audio_elements_for_duration_floor(timeline_nsecs, sample_rate, channels)
}

pub(in crate::player::backend::ffmpeg) fn audio_elements_for_duration_floor(
    timeline_nsecs: u64,
    sample_rate: c_int,
    channels: c_int,
) -> u64 {
    audio_elements_for_frames(
        audio_frames_for_duration_floor(timeline_nsecs, sample_rate),
        channels,
    )
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn audio_elements_for_duration_round(
    timeline_nsecs: u64,
    sample_rate: c_int,
    channels: c_int,
) -> u64 {
    audio_elements_for_frames(
        audio_frames_for_duration_round(timeline_nsecs, sample_rate),
        channels,
    )
}

pub(in crate::player::backend::ffmpeg) fn audio_frames_for_duration_floor(
    timeline_nsecs: u64,
    sample_rate: c_int,
) -> u64 {
    if timeline_nsecs == 0 || sample_rate <= 0 {
        return 0;
    }

    let frames = (timeline_nsecs as u128).saturating_mul(sample_rate as u128) / 1_000_000_000;
    u64::try_from(frames).unwrap_or(u64::MAX)
}

pub(in crate::player::backend::ffmpeg) fn audio_frames_for_duration_round(
    timeline_nsecs: u64,
    sample_rate: c_int,
) -> u64 {
    if timeline_nsecs == 0 || sample_rate <= 0 {
        return 0;
    }

    let numerator = (timeline_nsecs as u128).saturating_mul(sample_rate as u128);
    let frames = numerator.saturating_add(500_000_000) / 1_000_000_000;
    u64::try_from(frames).unwrap_or(u64::MAX)
}

pub(in crate::player::backend::ffmpeg) fn audio_elements_for_frames(
    frames: u64,
    channels: c_int,
) -> u64 {
    if frames == 0 || channels <= 0 {
        return 0;
    }

    frames.saturating_mul(channels as u64)
}

pub(in crate::player::backend::ffmpeg) fn audio_frames_for_elements(
    elements: usize,
    channels: c_int,
) -> u64 {
    if elements == 0 || channels <= 0 {
        return 0;
    }

    u64::try_from(elements / usize::try_from(channels).unwrap_or(usize::MAX)).unwrap_or(u64::MAX)
}

pub(in crate::player::backend::ffmpeg) fn align_audio_elements_to_frame_boundary(
    elements: usize,
    channels: c_int,
) -> usize {
    if elements == 0 || channels <= 0 {
        return 0;
    }

    let channels = usize::try_from(channels).unwrap_or(usize::MAX);
    elements.saturating_sub(elements % channels)
}
