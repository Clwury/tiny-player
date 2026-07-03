use std::{sync::Arc, thread, time::Instant};

use ffmpeg_sys_next as ffi;

use super::{
    AvPacket, DEMUX_PACKET_CACHE_WAIT_INTERVAL, DEMUX_READ_SLOW_LOG_AFTER, DemuxPacketCacheShared,
    DemuxPacketCacheThreadInput, DemuxPacketTimeline, ffmpeg_error,
    playback_buffered_near_duration, preroll_seek_position_seconds, video_seek_preroll_nsecs,
};

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
            continue;
        }
        timeline.set_session_id(shared.session_id());
        if read >= 0 {
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

        tracing::debug!(
            read_result = read,
            error = %ffmpeg_error(read),
            generation,
            seek_generation,
            pending_seek = shared.control.has_pending_seek(),
            buffered_until = ?timeline.buffered_until(),
            "FFmpeg demux av_read_frame returned error"
        );
        if shared.control.has_pending_seek() {
            thread::yield_now();
            continue;
        }
        if read == ffi::AVERROR_EOF
            || (read == ffi::AVERROR(ffi::EIO)
                && playback_buffered_near_duration(duration_seconds, timeline.buffered_until()))
        {
            timeline.buffered_reporter.report_value(
                duration_seconds,
                timeline.session_id,
                &shared.event_tx,
            );
            shared.mark_eof();
            continue;
        }
        if read == ffi::AVERROR(ffi::EAGAIN) {
            thread::sleep(DEMUX_PACKET_CACHE_WAIT_INTERVAL);
            continue;
        }
        shared.set_error(format!("FFmpeg 读取媒体包失败：{}", ffmpeg_error(read)));
    }
}
