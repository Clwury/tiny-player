use std::{os::raw::c_int, sync::mpsc::Sender};

use ffmpeg_sys_next as ffi;

use super::{
    AvPacket, BackendEvent, BufferedReporter, CachedDemuxPacket, CachedDemuxPacketRecovery,
    DEFAULT_VIDEO_FRAME_DURATION_NSECS, DemuxSelectedStreams, PlaybackSessionId, StreamInfo,
    TimestampMapper, VideoRecoveryPointKind, packet_duration_nsecs, packet_is_audio_recovery_point,
    packet_is_video_seek_point, packet_video_recovery_point_kind, seconds_to_nsecs,
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
        let (start_nsecs, end_nsecs) = self.packet_timeline_range(packet);
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

    fn packet_timeline_range(&mut self, packet: &AvPacket) -> (Option<u64>, Option<u64>) {
        if packet.stream_index() == self.video_stream.index {
            let timestamp = packet.best_timestamp().unwrap_or(ffi::AV_NOPTS_VALUE);
            let mapped = self.video_clock.map(timestamp, self.video_stream.time_base);
            let duration_nsecs = packet_duration_nsecs(packet, self.video_stream)
                .unwrap_or(self.video_frame_duration_nsecs);
            let end_nsecs = mapped.timeline_nsecs.saturating_add(duration_nsecs);
            return (Some(mapped.timeline_nsecs), Some(end_nsecs));
        }
        let Some(audio_stream) = self
            .audio_stream
            .filter(|stream| packet.stream_index() == stream.index)
        else {
            let Some(subtitle_stream) = self
                .subtitle_stream
                .filter(|stream| packet.stream_index() == stream.index)
            else {
                return (None, None);
            };
            let timestamp = packet.best_timestamp().unwrap_or(ffi::AV_NOPTS_VALUE);
            let mapped = self
                .subtitle_clock
                .map(timestamp, subtitle_stream.time_base);
            let end_nsecs = packet_duration_nsecs(packet, subtitle_stream)
                .map(|duration| mapped.timeline_nsecs.saturating_add(duration))
                .or(Some(mapped.timeline_nsecs));
            return (Some(mapped.timeline_nsecs), end_nsecs);
        };
        let timestamp = packet.best_timestamp().unwrap_or(ffi::AV_NOPTS_VALUE);
        let mapped = self.audio_clock.map(timestamp, audio_stream.time_base);
        let end_nsecs = packet_duration_nsecs(packet, audio_stream)
            .map(|duration| mapped.timeline_nsecs.saturating_add(duration));
        (Some(mapped.timeline_nsecs), end_nsecs)
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
