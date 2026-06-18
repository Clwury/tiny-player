use std::{
    os::raw::c_int,
    time::{Duration, Instant},
};

use crate::player::backend::StreamCacheKind;

use super::DemuxPacketCacheState;

#[derive(Clone, Copy, Debug, Default)]
pub(in crate::player::backend::ffmpeg::playback_loop) struct DemuxPacketCacheReadTiming {
    pub(in crate::player::backend::ffmpeg::playback_loop) lock_wait: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop) try_lock_failures: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop) lock_timed_out: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop) data_wait: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop) data_waits: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop) take_packet: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop) advance_reader_head: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop) refresh_reader_tracking: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop) trim: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop) forward_bytes: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop) forward_window: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop) packet_ref: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop) disk_read: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop) disk_reads: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop) struct DemuxPacketQueueSnapshot {
    pub(in crate::player::backend::ffmpeg::playback_loop) total_packets: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop) total_bytes: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop) memory_limit_bytes: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop) streams:
        Vec<DemuxStreamPacketQueueSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop) struct DemuxStreamPacketQueueSnapshot {
    pub(in crate::player::backend::ffmpeg::playback_loop) stream_index: c_int,
    pub(in crate::player::backend::ffmpeg::playback_loop) kind: StreamCacheKind,
    pub(in crate::player::backend::ffmpeg::playback_loop) queued_packets: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop) packet_limit: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop) packet_queue_full: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop) queued_bytes: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop) forward_nsecs: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct DemuxReaderWatermark {
    pub(in crate::player::backend::ffmpeg) video_forward_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) audio_forward_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) selected_min_forward_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) video_underrun: bool,
    pub(in crate::player::backend::ffmpeg) audio_underrun: bool,
    pub(in crate::player::backend::ffmpeg) video_idle: bool,
    pub(in crate::player::backend::ffmpeg) audio_idle: bool,
    pub(in crate::player::backend::ffmpeg) underrun: bool,
    pub(in crate::player::backend::ffmpeg) idle: bool,
    pub(in crate::player::backend::ffmpeg) forward_bytes: u64,
}

#[derive(Clone, Debug, Default)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxPacketCacheMonitorSnapshot
{
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) packet_queue:
        DemuxPacketQueueSnapshot,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) reader_watermark:
        DemuxReaderWatermark,
}

impl DemuxPacketCacheMonitorSnapshot {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn from_state(
        state: &DemuxPacketCacheState,
    ) -> Self {
        Self {
            packet_queue: state.packet_queue_snapshot(),
            reader_watermark: state.reader_watermark(),
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn from_state_with_timing(
        state: &DemuxPacketCacheState,
        timing: &mut DemuxPacketCacheReadTiming,
    ) -> Self {
        let packet_queue_started_at = Instant::now();
        let packet_queue = state.packet_queue_snapshot();
        timing.forward_window += packet_queue_started_at.elapsed();

        let reader_watermark_started_at = Instant::now();
        let reader_watermark = state.reader_watermark();
        timing.forward_bytes += reader_watermark_started_at.elapsed();

        Self {
            packet_queue,
            reader_watermark,
        }
    }
}
