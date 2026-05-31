use super::decode::DecodePacketAdmissionStatus;
use super::playback_pipeline_state::{DecoderInputSnapshot, PlaybackPipelineState};
use super::video_decode_pipeline::VideoPacketAdmissionPressure;
use super::*;

const DEMUX_PACKET_PUMP_MAX_PACKETS_PER_TICK: usize = 16;

#[derive(Default)]
pub(super) struct DemuxPacketPump {
    stream_cursor: usize,
}

struct DemuxPacketPumpContext<'a> {
    session_id: PlaybackSessionId,
    demux_cache: &'a DemuxPacketCache,
    decoder_input: &'a DecoderInputSnapshot,
    should_wait_for_demux: bool,
    video_output_waiting_for_demux: bool,
}

pub(super) struct DemuxPacketPumpAdmissionContext<'a> {
    pub(super) session_id: PlaybackSessionId,
    pub(super) demux_cache: &'a DemuxPacketCache,
    pub(super) pipeline: &'a mut PlaybackPipelineState,
    pub(super) video_admission_pressure: VideoPacketAdmissionPressure,
    pub(super) should_wait_for_demux: bool,
    pub(super) video_output_waiting_for_demux: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DemuxPacketRoute {
    Video,
    Audio,
    Subtitle,
    Other,
}

pub(super) enum DemuxPacketPumpResult {
    Progress,
    Backpressured,
    Eof,
    WouldBlock,
    Interrupted,
    Error(String),
}

impl DemuxPacketPump {
    fn poll_packet(&mut self, context: DemuxPacketPumpContext<'_>) -> DemuxReadResult {
        let mut demux_streams = context.decoder_input.demux_streams.clone();
        let demux_stream_rotation = if demux_streams.is_empty() {
            0
        } else {
            self.stream_cursor % demux_streams.len()
        };
        demux_streams.rotate_left(demux_stream_rotation);

        let demux_read_started_at = Instant::now();
        let (demux_read_result, demux_consumed_stream_offset) =
            Self::poll_per_stream(context.demux_cache, &demux_streams);
        let demux_read_elapsed = demux_read_started_at.elapsed();
        if demux_read_elapsed >= DEMUX_READ_WAIT_LOG_AFTER {
            self.log_wait(
                &context,
                &demux_streams,
                demux_read_elapsed,
                &demux_read_result,
            );
        }

        if matches!(demux_read_result, DemuxReadResult::Packet(_))
            && !demux_streams.is_empty()
            && let Some(consumed_offset) = demux_consumed_stream_offset
        {
            self.stream_cursor =
                (demux_stream_rotation + consumed_offset + 1) % demux_streams.len();
        }

        demux_read_result
    }

    pub(super) fn poll_and_admit_packet(
        &mut self,
        mut context: DemuxPacketPumpAdmissionContext<'_>,
    ) -> DemuxPacketPumpResult {
        let mut made_progress = false;
        for _ in 0..DEMUX_PACKET_PUMP_MAX_PACKETS_PER_TICK {
            match self.poll_and_admit_one(&mut context) {
                DemuxPacketPumpResult::Progress => made_progress = true,
                DemuxPacketPumpResult::Backpressured => {
                    return DemuxPacketPumpResult::Backpressured;
                }
                DemuxPacketPumpResult::Eof => {
                    return if made_progress {
                        DemuxPacketPumpResult::Progress
                    } else {
                        DemuxPacketPumpResult::Eof
                    };
                }
                DemuxPacketPumpResult::WouldBlock => {
                    return if made_progress {
                        DemuxPacketPumpResult::Progress
                    } else {
                        DemuxPacketPumpResult::WouldBlock
                    };
                }
                DemuxPacketPumpResult::Interrupted => return DemuxPacketPumpResult::Interrupted,
                DemuxPacketPumpResult::Error(error) => return DemuxPacketPumpResult::Error(error),
            }
        }
        DemuxPacketPumpResult::Progress
    }

    fn poll_and_admit_one(
        &mut self,
        context: &mut DemuxPacketPumpAdmissionContext<'_>,
    ) -> DemuxPacketPumpResult {
        let decoder_input = context.pipeline.decoder_input_snapshot();
        let demux_read_result = self.poll_packet(DemuxPacketPumpContext {
            session_id: context.session_id,
            demux_cache: context.demux_cache,
            decoder_input: &decoder_input,
            should_wait_for_demux: context.should_wait_for_demux,
            video_output_waiting_for_demux: context.video_output_waiting_for_demux,
        });
        let mut packet = match demux_read_result {
            DemuxReadResult::Packet(packet) => packet,
            DemuxReadResult::Eof => return DemuxPacketPumpResult::Eof,
            DemuxReadResult::WouldBlock => return DemuxPacketPumpResult::WouldBlock,
            DemuxReadResult::Interrupted => return DemuxPacketPumpResult::Interrupted,
            DemuxReadResult::Error(error) => return DemuxPacketPumpResult::Error(error),
        };

        let route = self.route_packet(
            &packet,
            decoder_input.video_stream_index,
            decoder_input.audio_stream_index,
            decoder_input.subtitle_stream_index,
        );
        let process_result: std::result::Result<DecodePacketAdmissionStatus, String> = match route {
            DemuxPacketRoute::Video => context.pipeline.admit_video_demux_packet(
                &packet,
                context.session_id,
                context.video_admission_pressure,
            ),
            DemuxPacketRoute::Audio => context
                .pipeline
                .admit_audio_demux_packet(&packet, context.session_id),
            DemuxPacketRoute::Subtitle => context
                .pipeline
                .admit_subtitle_demux_packet(&packet, context.session_id),
            DemuxPacketRoute::Other => Ok(DecodePacketAdmissionStatus::Dropped),
        };
        packet.unref();

        match process_result {
            Ok(status) if status.backpressured() => DemuxPacketPumpResult::Backpressured,
            Ok(_) => DemuxPacketPumpResult::Progress,
            Err(error) => DemuxPacketPumpResult::Error(error),
        }
    }

    fn route_packet(
        &self,
        packet: &AvPacket,
        video_stream_index: c_int,
        audio_stream_index: Option<c_int>,
        subtitle_stream_index: Option<c_int>,
    ) -> DemuxPacketRoute {
        let stream_index = packet.stream_index();
        if stream_index == video_stream_index {
            DemuxPacketRoute::Video
        } else if audio_stream_index == Some(stream_index) {
            DemuxPacketRoute::Audio
        } else if subtitle_stream_index == Some(stream_index) {
            DemuxPacketRoute::Subtitle
        } else {
            DemuxPacketRoute::Other
        }
    }

    fn poll_per_stream(
        demux_cache: &DemuxPacketCache,
        demux_streams: &[c_int],
    ) -> (DemuxReadResult, Option<usize>) {
        let mut saw_would_block = false;
        let mut saw_eof = false;
        for (stream_offset, stream_index) in demux_streams.iter().copied().enumerate() {
            match demux_cache.poll_packet(stream_index) {
                DemuxReadResult::Packet(packet) => {
                    return (DemuxReadResult::Packet(packet), Some(stream_offset));
                }
                DemuxReadResult::Eof => saw_eof = true,
                DemuxReadResult::WouldBlock => saw_would_block = true,
                DemuxReadResult::Interrupted => return (DemuxReadResult::Interrupted, None),
                DemuxReadResult::Error(error) => return (DemuxReadResult::Error(error), None),
            }
        }
        if saw_eof && !saw_would_block {
            (DemuxReadResult::Eof, None)
        } else {
            (DemuxReadResult::WouldBlock, None)
        }
    }

    fn log_wait(
        &self,
        context: &DemuxPacketPumpContext<'_>,
        demux_streams: &[c_int],
        demux_read_elapsed: Duration,
        demux_read_result: &DemuxReadResult,
    ) {
        let result = match demux_read_result {
            DemuxReadResult::Packet(_) => "packet",
            DemuxReadResult::Eof => "eof",
            DemuxReadResult::WouldBlock => "would_block",
            DemuxReadResult::Interrupted => "interrupted",
            DemuxReadResult::Error(_) => "error",
        };
        let demux_packet_snapshot = context.demux_cache.packet_queue_snapshot();
        let demux_packet_queue_full = demux_packet_snapshot
            .streams
            .iter()
            .any(|stream| stream.packet_queue_full);
        let video_decode_snapshot = context.decoder_input.video_decode_snapshot;
        let video_decode_blocked_on = context.decoder_input.video_decode_blocked_on;
        let blocked_on = if matches!(
            video_decode_blocked_on,
            Some(
                PlaybackBlockReason::PacketQueueFull
                    | PlaybackBlockReason::DecodedQueueFull
                    | PlaybackBlockReason::HwSurfacePool
            )
        ) {
            video_decode_blocked_on.expect("video decode block reason checked above")
        } else if demux_packet_queue_full {
            PlaybackBlockReason::PacketQueueFull
        } else if context.should_wait_for_demux
            || matches!(demux_read_result, DemuxReadResult::WouldBlock)
        {
            video_decode_blocked_on.unwrap_or(PlaybackBlockReason::DemuxCache)
        } else {
            PlaybackBlockReason::OutputGate
        };
        tracing::debug!(
            session_id = ?context.session_id,
            blocked_on = blocked_on.as_str(),
            waited_ms = demux_read_elapsed.as_secs_f64() * 1000.0,
            result,
            video_output_waiting_for_demux = context.video_output_waiting_for_demux,
            should_wait_for_demux = context.should_wait_for_demux,
            demux_streams = ?demux_streams,
            demux_packet_queued = demux_packet_snapshot.total_packets,
            demux_packet_bytes = demux_packet_snapshot.total_bytes,
            demux_packet_queue_full,
            demux_packet_streams = ?demux_packet_snapshot.streams,
            video_decode_state = ?video_decode_snapshot.state,
            video_decode_queued_frames = video_decode_snapshot.queued_frames,
            video_decode_queue_capacity = video_decode_snapshot.queue_capacity,
            video_decode_pending_input_packets = video_decode_snapshot.pending_input_packets,
            video_decode_pending_input_capacity = video_decode_snapshot.pending_input_capacity,
            video_decode_pending_input_full = video_decode_snapshot.pending_input_full(),
            video_decode_in_flight_packets = video_decode_snapshot.in_flight_packets,
            video_decode_completed_packets = video_decode_snapshot.completed_packets,
            "FFmpeg demux packet read wait completed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet_for_stream(stream_index: c_int) -> AvPacket {
        let mut packet = AvPacket::new().expect("packet allocates");
        unsafe {
            (*packet.as_mut_ptr()).stream_index = stream_index;
        }
        packet
    }

    #[test]
    fn demux_packet_pump_routes_packets_by_selected_streams() {
        let pump = DemuxPacketPump::default();

        assert_eq!(
            pump.route_packet(&packet_for_stream(10), 10, Some(11), Some(12)),
            DemuxPacketRoute::Video
        );
        assert_eq!(
            pump.route_packet(&packet_for_stream(11), 10, Some(11), Some(12)),
            DemuxPacketRoute::Audio
        );
        assert_eq!(
            pump.route_packet(&packet_for_stream(12), 10, Some(11), Some(12)),
            DemuxPacketRoute::Subtitle
        );
        assert_eq!(
            pump.route_packet(&packet_for_stream(13), 10, Some(11), Some(12)),
            DemuxPacketRoute::Other
        );
    }

    #[test]
    fn demux_packet_pump_treats_unselected_audio_subtitle_as_other() {
        let pump = DemuxPacketPump::default();

        assert_eq!(
            pump.route_packet(&packet_for_stream(11), 10, None, Some(12)),
            DemuxPacketRoute::Other
        );
        assert_eq!(
            pump.route_packet(&packet_for_stream(12), 10, Some(11), None),
            DemuxPacketRoute::Other
        );
    }
}
