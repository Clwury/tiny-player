use std::{sync::Arc, thread, time::Instant};

use ffmpeg_sys_next as ffi;

use super::{
    AvPacket, DEMUX_PACKET_CACHE_WAIT_INTERVAL, DEMUX_READ_SLOW_LOG_AFTER, DemuxPacketCacheShared,
    DemuxPacketCacheThreadInput, DemuxPacketTimeline, ffmpeg_error,
    playback_buffered_near_duration, preroll_seek_position_seconds, video_seek_preroll_nsecs,
};

const DEMUX_READ_MAX_CONSECUTIVE_ERRORS: u32 = 10;
const DEMUX_PRODUCER_ERROR_LOW_WATER_NSECS: u64 = 250_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DemuxReadErrorAction {
    Retry,
    DrainCached,
    Fatal,
}

impl DemuxReadErrorAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::DrainCached => "drain_cached",
            Self::Fatal => "fatal",
        }
    }
}

#[derive(Default)]
struct DemuxReadRecovery {
    consecutive_errors: u32,
    last_input_progress_generation: Option<u64>,
}

impl DemuxReadRecovery {
    fn new(input_progress_generation: Option<u64>) -> Self {
        Self {
            last_input_progress_generation: input_progress_generation,
            ..Self::default()
        }
    }

    fn observe_error(&mut self, input_progress_generation: Option<u64>) -> bool {
        let input_progressed = self
            .last_input_progress_generation
            .zip(input_progress_generation)
            .is_some_and(|(previous, current)| previous != current);
        if input_progressed {
            self.consecutive_errors = 0;
        }
        self.last_input_progress_generation = input_progress_generation;
        self.consecutive_errors = self.consecutive_errors.saturating_add(1);
        input_progressed
    }

    fn reset(&mut self, input_progress_generation: Option<u64>) -> u32 {
        let previous = std::mem::take(&mut self.consecutive_errors);
        self.last_input_progress_generation = input_progress_generation;
        previous
    }
}

fn demux_read_error_action(
    consecutive_errors: u32,
    input_progressed: bool,
    consumer_drainable: bool,
    forward_duration_nsecs: u64,
) -> DemuxReadErrorAction {
    if consecutive_errors < DEMUX_READ_MAX_CONSECUTIVE_ERRORS || input_progressed {
        return DemuxReadErrorAction::Retry;
    }
    if consumer_drainable && forward_duration_nsecs > DEMUX_PRODUCER_ERROR_LOW_WATER_NSECS {
        DemuxReadErrorAction::DrainCached
    } else {
        DemuxReadErrorAction::Fatal
    }
}

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn run_demux_packet_cache(
    thread_input: DemuxPacketCacheThreadInput,
    shared: Arc<DemuxPacketCacheShared>,
) {
    let DemuxPacketCacheThreadInput {
        mut input,
        video_stream,
        audio_stream,
        subtitle_stream,
        duration_seconds,
        start_position_seconds,
        session_id,
    } = thread_input;
    let mut timeline = DemuxPacketTimeline::new(
        video_stream,
        audio_stream,
        subtitle_stream,
        start_position_seconds,
        session_id,
    );
    let cached_input = input.cached_io_cache();
    let mut read_recovery = DemuxReadRecovery::new(
        cached_input
            .as_ref()
            .map(|cache| cache.input_progress_generation()),
    );
    timeline.reset(start_position_seconds, session_id, &shared.event_tx);
    let mut packet = match AvPacket::new() {
        Ok(packet) => packet,
        Err(error) => {
            shared.set_error(error);
            return;
        }
    };

    loop {
        if shared.should_stop() {
            return;
        }
        let request = shared.wait_for_demux_permit();
        if shared.should_stop() {
            return;
        }
        timeline.set_selected_streams(shared.selected_streams());
        if let Some(request) = request {
            if shared.should_skip_seek_request(&request) {
                tracing::debug!(
                    ?request.session_id,
                    position_seconds = request.position_seconds,
                    request_seek_generation = request.seek_generation,
                    current_seek_generation = shared.control.seek_generation(),
                    "skipping stale FFmpeg demux low-level seek request"
                );
                continue;
            }
            let generation = shared.generation();
            let seek_generation = request.seek_generation;
            tracing::debug!(
                ?request.session_id,
                position_seconds = request.position_seconds,
                seek_position_seconds = preroll_seek_position_seconds(
                    video_stream.codec_id,
                    request.position_seconds
                ),
                preroll_nsecs = video_seek_preroll_nsecs(video_stream.codec_id),
                generation,
                seek_generation,
                "FFmpeg demux thread applying low-level seek"
            );
            if let Err(error) = input.seek_stream(
                video_stream,
                preroll_seek_position_seconds(video_stream.codec_id, request.position_seconds),
            ) {
                if shared.should_discard_demux_result(generation, seek_generation) {
                    tracing::debug!(
                        ?request.session_id,
                        position_seconds = request.position_seconds,
                        generation,
                        current_generation = shared.generation(),
                        seek_generation,
                        current_seek_generation = shared.control.seek_generation(),
                        %error,
                        "discarding FFmpeg demux seek error after newer seek"
                    );
                    continue;
                }
                shared.set_error(error);
                continue;
            }
            if shared.should_discard_demux_result(generation, seek_generation) {
                tracing::debug!(
                    ?request.session_id,
                    position_seconds = request.position_seconds,
                    generation,
                    current_generation = shared.generation(),
                    seek_generation,
                    current_seek_generation = shared.control.seek_generation(),
                    "discarding FFmpeg demux seek result after newer seek"
                );
                continue;
            }
            tracing::debug!(
                ?request.session_id,
                position_seconds = request.position_seconds,
                seek_position_seconds = preroll_seek_position_seconds(
                    video_stream.codec_id,
                    request.position_seconds
                ),
                generation,
                seek_generation,
                "FFmpeg demux thread low-level seek applied"
            );
            if read_recovery.reset(
                cached_input
                    .as_ref()
                    .map(|cache| cache.input_progress_generation()),
            ) > 0
            {
                shared.clear_producer_recovery();
            }
            timeline.reset(
                request.position_seconds,
                request.session_id,
                &shared.event_tx,
            );
        }

        let generation = shared.generation();
        let seek_generation = shared.control.seek_generation();
        if shared.control.has_pending_seek() {
            thread::yield_now();
            continue;
        }
        shared.mark_demux_read_started();
        let read_started_at = Instant::now();
        let read = unsafe { ffi::av_read_frame(input.as_mut_ptr(), packet.as_mut_ptr()) };
        let read_elapsed = read_started_at.elapsed();
        shared.mark_demux_read_finished();
        if read_elapsed >= DEMUX_READ_SLOW_LOG_AFTER {
            shared.log_slow_demux_read(read_elapsed, read);
        }
        if shared.should_stop() {
            packet.unref();
            break;
        }
        if shared.should_discard_demux_result(generation, seek_generation) {
            tracing::debug!(
                generation,
                current_generation = shared.generation(),
                seek_generation,
                current_seek_generation = shared.control.seek_generation(),
                read_result = read,
                "discarding FFmpeg demux read result after newer seek"
            );
            packet.unref();
            if read_recovery.reset(
                cached_input
                    .as_ref()
                    .map(|cache| cache.input_progress_generation()),
            ) > 0
            {
                shared.clear_producer_recovery();
            }
            continue;
        }
        timeline.set_session_id(shared.session_id());
        if read >= 0 {
            let recovered_errors = read_recovery.reset(
                cached_input
                    .as_ref()
                    .map(|cache| cache.input_progress_generation()),
            );
            if recovered_errors > 0 {
                shared.clear_producer_recovery();
                tracing::info!(
                    recovered_errors,
                    generation,
                    seek_generation,
                    "FFmpeg demux producer recovered after bounded read retries"
                );
            }
            match timeline.cache_packet(&packet, &shared.event_tx) {
                Ok(Some(cached)) => {
                    if shared.should_discard_demux_result(generation, seek_generation) {
                        tracing::debug!(
                            generation,
                            current_generation = shared.generation(),
                            seek_generation,
                            current_seek_generation = shared.control.seek_generation(),
                            "discarding FFmpeg demux packet before append after newer seek"
                        );
                    } else {
                        shared.append_packet(cached);
                    }
                }
                Ok(None) => {}
                Err(error) => shared.set_error(error),
            }
            packet.unref();
            // Yield after each appended packet so the coordinator pump — which feeds
            // the decoder under the same cache mutex — gets fair access. Without this,
            // a producer draining an already-buffered byte cache can starve the pump on
            // the non-fair mutex and throttle decode below realtime.
            thread::yield_now();
            continue;
        }
        packet.unref();

        if shared.control.has_pending_seek() {
            if read_recovery.reset(
                cached_input
                    .as_ref()
                    .map(|cache| cache.input_progress_generation()),
            ) > 0
            {
                shared.clear_producer_recovery();
            }
            thread::yield_now();
            continue;
        }
        if read == ffi::AVERROR_EOF
            || (read == ffi::AVERROR(ffi::EIO)
                && playback_buffered_near_duration(duration_seconds, timeline.buffered_until()))
        {
            if read_recovery.reset(
                cached_input
                    .as_ref()
                    .map(|cache| cache.input_progress_generation()),
            ) > 0
            {
                shared.clear_producer_recovery();
            }
            timeline.buffered_reporter.report_value(
                duration_seconds,
                timeline.session_id,
                &shared.event_tx,
            );
            shared.mark_eof();
            continue;
        }
        if read == ffi::AVERROR(ffi::EAGAIN) {
            let observed_generation = shared.control.wake_generation();
            shared
                .control
                .wait_for_wake_change(observed_generation, DEMUX_PACKET_CACHE_WAIT_INTERVAL);
            continue;
        }
        let error = ffmpeg_error(read);
        let input_progress_generation = cached_input
            .as_ref()
            .map(|cache| cache.input_progress_generation());
        let input_progressed = read_recovery.observe_error(input_progress_generation);
        let (consumer_drainable, forward_duration_nsecs) = shared.note_producer_recovering(
            format!("FFmpeg 读取媒体包失败：{error}"),
            read_recovery.consecutive_errors,
        );
        let action = demux_read_error_action(
            read_recovery.consecutive_errors,
            input_progressed,
            consumer_drainable,
            forward_duration_nsecs,
        );
        tracing::debug!(
            read_result = read,
            %error,
            generation,
            seek_generation,
            consecutive_errors = read_recovery.consecutive_errors,
            max_consecutive_errors = DEMUX_READ_MAX_CONSECUTIVE_ERRORS,
            input_progressed,
            input_progress_generation = ?input_progress_generation,
            consumer_drainable,
            forward_duration_ms = forward_duration_nsecs as f64 / 1_000_000.0,
            low_water_ms = DEMUX_PRODUCER_ERROR_LOW_WATER_NSECS as f64 / 1_000_000.0,
            recovery_action = action.as_str(),
            buffered_until = ?timeline.buffered_until(),
            "FFmpeg demux av_read_frame returned recoverable error"
        );
        if read_recovery.consecutive_errors == 1
            || read_recovery.consecutive_errors == DEMUX_READ_MAX_CONSECUTIVE_ERRORS
            || matches!(action, DemuxReadErrorAction::Fatal)
        {
            tracing::warn!(
                %error,
                consecutive_errors = read_recovery.consecutive_errors,
                consumer_drainable,
                forward_duration_ms = forward_duration_nsecs as f64 / 1_000_000.0,
                recovery_action = action.as_str(),
                "FFmpeg demux producer entered bounded recovery"
            );
        }
        if matches!(action, DemuxReadErrorAction::Fatal) {
            shared.set_error(format!("FFmpeg 读取媒体包失败：{error}"));
            continue;
        }
        if let (Some(cache), Some(observed_generation)) =
            (cached_input.as_ref(), input_progress_generation)
        {
            cache.wait_for_input_progress_change(
                observed_generation,
                DEMUX_PACKET_CACHE_WAIT_INTERVAL,
            );
        } else {
            let observed_generation = shared.control.wake_generation();
            shared
                .control
                .wait_for_wake_change(observed_generation, DEMUX_PACKET_CACHE_WAIT_INTERVAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEMUX_PRODUCER_ERROR_LOW_WATER_NSECS, DEMUX_READ_MAX_CONSECUTIVE_ERRORS,
        DemuxReadErrorAction, DemuxReadRecovery, demux_read_error_action,
    };

    #[test]
    fn demux_read_recovery_resets_error_count_on_http_input_progress() {
        let mut recovery = DemuxReadRecovery::new(Some(7));
        assert!(!recovery.observe_error(Some(7)));
        assert_eq!(recovery.consecutive_errors, 1);
        assert!(recovery.observe_error(Some(8)));
        assert_eq!(recovery.consecutive_errors, 1);
    }

    #[test]
    fn demux_read_recovery_discards_pre_seek_error_sequence() {
        let mut recovery = DemuxReadRecovery::new(Some(7));
        assert!(!recovery.observe_error(Some(7)));
        assert_eq!(recovery.reset(Some(7)), 1);
        assert_eq!(recovery.consecutive_errors, 0);
    }

    #[test]
    fn demux_read_recovery_defers_fatal_error_while_packet_cache_is_drainable() {
        assert_eq!(
            demux_read_error_action(
                DEMUX_READ_MAX_CONSECUTIVE_ERRORS,
                false,
                true,
                DEMUX_PRODUCER_ERROR_LOW_WATER_NSECS + 1,
            ),
            DemuxReadErrorAction::DrainCached
        );
    }

    #[test]
    fn demux_read_recovery_becomes_fatal_only_after_retries_at_low_water() {
        assert_eq!(
            demux_read_error_action(DEMUX_READ_MAX_CONSECUTIVE_ERRORS - 1, false, false, 0,),
            DemuxReadErrorAction::Retry
        );
        assert_eq!(
            demux_read_error_action(
                DEMUX_READ_MAX_CONSECUTIVE_ERRORS,
                false,
                true,
                DEMUX_PRODUCER_ERROR_LOW_WATER_NSECS,
            ),
            DemuxReadErrorAction::Fatal
        );
    }
}
