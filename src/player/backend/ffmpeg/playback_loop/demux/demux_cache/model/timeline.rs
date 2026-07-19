use std::{os::raw::c_int, sync::mpsc::Sender};

use ffmpeg_sys_next as ffi;

use super::{
    AvPacket, BackendEvent, BufferedReporter, CachedDemuxPacket, CachedDemuxPacketRecovery,
    DEFAULT_VIDEO_FRAME_DURATION_NSECS, DemuxSelectedStreams, PlaybackSessionId, StreamInfo,
    TimestampMapper, VideoRecoveryPointKind, packet_duration_nsecs, packet_is_audio_recovery_point,
    packet_is_video_seek_point, packet_video_recovery_point_kind, seconds_to_nsecs,
    timestamp_to_nsecs,
};

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxPacketTimeline {
    video_stream: StreamInfo,
    audio_stream: Option<StreamInfo>,
    subtitle_stream: Option<StreamInfo>,
    video_frame_duration_nsecs: u64,
    current_start_position_nsecs: u64,
    video_clock: TimestampMapper,
    audio_clock: TimestampMapper,
    subtitle_clock: TimestampMapper,
    video_seek_clock: DemuxSeekTimestampMapper,
    audio_seek_clock: DemuxSeekTimestampMapper,
    subtitle_seek_clock: DemuxSeekTimestampMapper,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) buffered_reporter:
        BufferedReporter,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) session_id:
        PlaybackSessionId,
}

impl DemuxPacketTimeline {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn new(
        video_stream: StreamInfo,
        audio_stream: Option<StreamInfo>,
        subtitle_stream: Option<StreamInfo>,
        start_position_seconds: f64,
        session_id: PlaybackSessionId,
    ) -> Self {
        let video_frame_duration_nsecs = video_stream
            .frame_duration_nsecs
            .unwrap_or(DEFAULT_VIDEO_FRAME_DURATION_NSECS);
        let current_start_position_nsecs = seconds_to_nsecs(start_position_seconds.max(0.0));
        let buffered_reporter = BufferedReporter::new_with_events(false, false);
        Self {
            video_stream,
            audio_stream,
            subtitle_stream,
            video_frame_duration_nsecs,
            current_start_position_nsecs,
            video_clock: TimestampMapper::new(
                video_stream.start_nsecs,
                current_start_position_nsecs,
                Some(video_frame_duration_nsecs),
            ),
            audio_clock: TimestampMapper::new(
                audio_stream.and_then(|stream| stream.start_nsecs),
                current_start_position_nsecs,
                None,
            ),
            subtitle_clock: TimestampMapper::new(
                subtitle_stream.and_then(|stream| stream.start_nsecs),
                current_start_position_nsecs,
                None,
            ),
            video_seek_clock: DemuxSeekTimestampMapper::new(
                video_stream.start_nsecs,
                current_start_position_nsecs,
            ),
            audio_seek_clock: DemuxSeekTimestampMapper::new(
                audio_stream.and_then(|stream| stream.start_nsecs),
                current_start_position_nsecs,
            ),
            subtitle_seek_clock: DemuxSeekTimestampMapper::new(
                subtitle_stream.and_then(|stream| stream.start_nsecs),
                current_start_position_nsecs,
            ),
            buffered_reporter,
            session_id,
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn set_selected_streams(
        &mut self,
        selected_streams: DemuxSelectedStreams,
    ) {
        let audio_unchanged = self.audio_stream.map(|stream| stream.index)
            == selected_streams.audio_stream.map(|stream| stream.index);
        let subtitle_unchanged = self.subtitle_stream.map(|stream| stream.index)
            == selected_streams.subtitle_stream.map(|stream| stream.index);
        if audio_unchanged && subtitle_unchanged {
            return;
        }
        self.audio_stream = selected_streams.audio_stream;
        self.subtitle_stream = selected_streams.subtitle_stream;
        self.audio_clock = TimestampMapper::new(
            self.audio_stream.and_then(|stream| stream.start_nsecs),
            self.current_start_position_nsecs,
            None,
        );
        self.subtitle_clock = TimestampMapper::new(
            self.subtitle_stream.and_then(|stream| stream.start_nsecs),
            self.current_start_position_nsecs,
            None,
        );
        self.audio_seek_clock = DemuxSeekTimestampMapper::new(
            self.audio_stream.and_then(|stream| stream.start_nsecs),
            self.current_start_position_nsecs,
        );
        self.subtitle_seek_clock = DemuxSeekTimestampMapper::new(
            self.subtitle_stream.and_then(|stream| stream.start_nsecs),
            self.current_start_position_nsecs,
        );
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn reset(
        &mut self,
        position_seconds: f64,
        session_id: PlaybackSessionId,
        event_tx: &Sender<BackendEvent>,
    ) {
        let position_seconds = position_seconds.max(0.0);
        self.session_id = session_id;
        self.current_start_position_nsecs = seconds_to_nsecs(position_seconds);
        self.video_clock = TimestampMapper::new(
            self.video_stream.start_nsecs,
            self.current_start_position_nsecs,
            Some(self.video_frame_duration_nsecs),
        );
        self.audio_clock = TimestampMapper::new(
            self.audio_stream.and_then(|stream| stream.start_nsecs),
            self.current_start_position_nsecs,
            None,
        );
        self.subtitle_clock = TimestampMapper::new(
            self.subtitle_stream.and_then(|stream| stream.start_nsecs),
            self.current_start_position_nsecs,
            None,
        );
        self.video_seek_clock = DemuxSeekTimestampMapper::new(
            self.video_stream.start_nsecs,
            self.current_start_position_nsecs,
        );
        self.audio_seek_clock = DemuxSeekTimestampMapper::new(
            self.audio_stream.and_then(|stream| stream.start_nsecs),
            self.current_start_position_nsecs,
        );
        self.subtitle_seek_clock = DemuxSeekTimestampMapper::new(
            self.subtitle_stream.and_then(|stream| stream.start_nsecs),
            self.current_start_position_nsecs,
        );
        self.buffered_reporter = BufferedReporter::new_with_events(false, false);
        self.buffered_reporter
            .reset_to(position_seconds, session_id, event_tx);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn cache_packet(
        &mut self,
        packet: &AvPacket,
        event_tx: &Sender<BackendEvent>,
    ) -> std::result::Result<Option<CachedDemuxPacket>, String> {
        if !self.should_cache_stream(packet.stream_index()) {
            return Ok(None);
        }
        let timestamps = self.packet_timestamps(packet);
        let start_nsecs = timestamps.start_nsecs;
        let end_nsecs = timestamps.end_nsecs;
        if packet.stream_index() == self.video_stream.index
            && let Some(end_nsecs) = end_nsecs
        {
            self.buffered_reporter.report_video_timeline_nsecs(
                end_nsecs,
                self.session_id,
                event_tx,
            );
        }
        let video_recovery_kind = if packet.stream_index() == self.video_stream.index {
            packet_video_recovery_point_kind(packet, self.video_stream.codec_id)
        } else {
            VideoRecoveryPointKind::None
        };
        let audio_recovery_point = self.audio_stream.is_some_and(|stream| {
            packet.stream_index() == stream.index
                && packet_is_audio_recovery_point(packet, stream.codec_id)
        });
        CachedDemuxPacket::from_packet(
            packet,
            packet.stream_index(),
            packet.stream_index() == self.video_stream.index,
            CachedDemuxPacketRecovery {
                recovery_point: video_recovery_kind.is_recovery_point() || audio_recovery_point,
                recovery_kind: video_recovery_kind,
                safe_seek_point: packet.stream_index() == self.video_stream.index
                    && packet_is_video_seek_point(packet, self.video_stream.codec_id),
            },
            start_nsecs,
            end_nsecs,
            timestamps.seek_timestamp_nsecs,
        )
        .map(Some)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn should_cache_stream(
        &self,
        stream_index: c_int,
    ) -> bool {
        stream_index == self.video_stream.index
            || self
                .audio_stream
                .is_some_and(|stream| stream.index == stream_index)
            || self
                .subtitle_stream
                .is_some_and(|stream| stream.index == stream_index)
    }

    fn packet_timestamps(&mut self, packet: &AvPacket) -> DemuxPacketTimestamps {
        if packet.stream_index() == self.video_stream.index {
            let raw_timestamp = packet.best_timestamp();
            let timestamp = raw_timestamp.unwrap_or(ffi::AV_NOPTS_VALUE);
            let mapped = self.video_clock.map(timestamp, self.video_stream.time_base);
            let seek_timestamp_nsecs = self
                .video_seek_clock
                .map(raw_timestamp, self.video_stream.time_base);
            let end_nsecs = packet_end_timeline_nsecs(
                packet,
                self.video_stream,
                timestamp,
                mapped.timeline_nsecs,
                Some(self.video_frame_duration_nsecs),
            )
            .unwrap_or(mapped.timeline_nsecs);
            return DemuxPacketTimestamps {
                start_nsecs: Some(mapped.timeline_nsecs),
                end_nsecs: Some(end_nsecs),
                seek_timestamp_nsecs,
            };
        }
        let Some(audio_stream) = self
            .audio_stream
            .filter(|stream| packet.stream_index() == stream.index)
        else {
            let Some(subtitle_stream) = self
                .subtitle_stream
                .filter(|stream| packet.stream_index() == stream.index)
            else {
                return DemuxPacketTimestamps::default();
            };
            let raw_timestamp = packet.best_timestamp();
            let timestamp = raw_timestamp.unwrap_or(ffi::AV_NOPTS_VALUE);
            let mapped = self
                .subtitle_clock
                .map(timestamp, subtitle_stream.time_base);
            let seek_timestamp_nsecs = self
                .subtitle_seek_clock
                .map(raw_timestamp, subtitle_stream.time_base);
            let end_nsecs = packet_end_timeline_nsecs(
                packet,
                subtitle_stream,
                timestamp,
                mapped.timeline_nsecs,
                Some(0),
            );
            return DemuxPacketTimestamps {
                start_nsecs: Some(mapped.timeline_nsecs),
                end_nsecs,
                seek_timestamp_nsecs,
            };
        };
        let raw_timestamp = packet.best_timestamp();
        let timestamp = raw_timestamp.unwrap_or(ffi::AV_NOPTS_VALUE);
        let mapped = self.audio_clock.map(timestamp, audio_stream.time_base);
        let seek_timestamp_nsecs = self
            .audio_seek_clock
            .map(raw_timestamp, audio_stream.time_base);
        let end_nsecs =
            packet_end_timeline_nsecs(packet, audio_stream, timestamp, mapped.timeline_nsecs, None);
        DemuxPacketTimestamps {
            start_nsecs: Some(mapped.timeline_nsecs),
            end_nsecs,
            seek_timestamp_nsecs,
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn buffered_until(
        &self,
    ) -> Option<f64> {
        self.buffered_reporter.buffered_until()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn set_session_id(
        &mut self,
        session_id: PlaybackSessionId,
    ) {
        self.session_id = session_id;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DemuxPacketTimestamps {
    start_nsecs: Option<u64>,
    end_nsecs: Option<u64>,
    seek_timestamp_nsecs: Option<u64>,
}

/// Maps valid packet PTS/DTS to the media timeline without rewriting legal
/// presentation reordering. Missing timestamps stay missing, matching mpv's
/// `compute_keyframe_times()` behavior for `MP_NOPTS_VALUE` packets.
#[derive(Clone)]
struct DemuxSeekTimestampMapper {
    stream_start_nsecs: Option<u64>,
    fallback_first_nsecs: Option<u64>,
    start_position_nsecs: u64,
}

impl DemuxSeekTimestampMapper {
    fn new(stream_start_nsecs: Option<u64>, start_position_nsecs: u64) -> Self {
        Self {
            stream_start_nsecs,
            fallback_first_nsecs: None,
            start_position_nsecs,
        }
    }

    fn map(&mut self, timestamp: Option<i64>, time_base: ffi::AVRational) -> Option<u64> {
        let timestamp_nsecs = timestamp_to_nsecs(timestamp?, time_base)?;
        if let Some(stream_start_nsecs) = self.stream_start_nsecs {
            return Some(timestamp_nsecs.saturating_sub(stream_start_nsecs));
        }

        let first_nsecs = *self.fallback_first_nsecs.get_or_insert(timestamp_nsecs);
        Some(if timestamp_nsecs >= first_nsecs {
            self.start_position_nsecs
                .saturating_add(timestamp_nsecs - first_nsecs)
        } else {
            self.start_position_nsecs
                .saturating_sub(first_nsecs - timestamp_nsecs)
        })
    }
}

fn packet_end_timeline_nsecs(
    packet: &AvPacket,
    stream: StreamInfo,
    timestamp: i64,
    start_timeline_nsecs: u64,
    fallback_duration_nsecs: Option<u64>,
) -> Option<u64> {
    // Rescaling an absolute timestamp and a duration independently is not
    // additive for rational time bases. Convert timestamp + duration instead,
    // then apply that exact delta to the already-mapped packet start. This
    // keeps adjacent packet boundaries identical without accumulating drift.
    let exact_duration_nsecs = packet.duration().and_then(|duration| {
        let end_timestamp = timestamp.checked_add(duration)?;
        let start_nsecs = timestamp_to_nsecs(timestamp, stream.time_base)?;
        let end_nsecs = timestamp_to_nsecs(end_timestamp, stream.time_base)?;
        end_nsecs.checked_sub(start_nsecs)
    });
    exact_duration_nsecs
        .or_else(|| packet_duration_nsecs(packet, stream))
        .or(fallback_duration_nsecs)
        .map(|duration| start_timeline_nsecs.saturating_add(duration))
}
