use std::time::{Duration, Instant};

use crate::player::backend::{DemuxCacheState, PlaybackCacheTimeRange};

use super::{
    AvPacket, FormatContext, PlaybackCacheConfig, PlaybackCacheState, PlaybackSessionId, StreamInfo,
};

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) type PacketId = u64;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) type RangeId = u64;

pub(in crate::player::backend::ffmpeg::playback_loop) struct DemuxPacketCacheInput {
    pub(in crate::player::backend::ffmpeg::playback_loop) input: FormatContext,
    pub(in crate::player::backend::ffmpeg::playback_loop) video_stream: StreamInfo,
    pub(in crate::player::backend::ffmpeg::playback_loop) audio_stream: Option<StreamInfo>,
    pub(in crate::player::backend::ffmpeg::playback_loop) subtitle_stream: Option<StreamInfo>,
    pub(in crate::player::backend::ffmpeg::playback_loop) duration_seconds: Option<f64>,
    pub(in crate::player::backend::ffmpeg::playback_loop) start_position_seconds: f64,
    pub(in crate::player::backend::ffmpeg::playback_loop) session_id: PlaybackSessionId,
    pub(in crate::player::backend::ffmpeg::playback_loop) cache_config: PlaybackCacheConfig,
}

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxPacketCacheThreadInput
{
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) input: FormatContext,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) video_stream: StreamInfo,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) audio_stream:
        Option<StreamInfo>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) subtitle_stream:
        Option<StreamInfo>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) duration_seconds:
        Option<f64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) start_position_seconds: f64,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) session_id:
        PlaybackSessionId,
}

pub(in crate::player::backend::ffmpeg::playback_loop) enum DemuxReadResult {
    Packet(AvPacket),
    Eof,
    WouldBlock,
    Interrupted,
    Error(String),
}

#[derive(Clone, Copy)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) enum DemuxCacheLockWait {
    None,
    Bounded(Duration),
    Unbounded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop) enum DemuxSeekResult {
    Cached,
    Requested,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop) struct DemuxStreamReaderRealignResult {
    pub(in crate::player::backend::ffmpeg::playback_loop) stream_index: i32,
    pub(in crate::player::backend::ffmpeg::playback_loop) target_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop) old_packet_id: Option<PacketId>,
    pub(in crate::player::backend::ffmpeg::playback_loop) old_start_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop) new_packet_id: PacketId,
    pub(in crate::player::backend::ffmpeg::playback_loop) new_start_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop) new_end_nsecs: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxPacketAppendTiming {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) lock_wait: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) lock_hold: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) record_input_bytes: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) skip_resume_overlap:
        Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) packet_index: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) disk_write: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) queue_insert: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) maybe_start_reader_head:
        Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) trim: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) refresh_readahead_hysteresis:
        Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) should_pause_demux: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) refresh_cache_pause:
        Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) emit_state: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) emit_state_lock_wait:
        Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) emit_state_prepare: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) emit_state_send: Duration,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) notify: Duration,
}

#[derive(Clone, Copy, Debug, Default)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxPacketAppendOutcome {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) appended: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) force_cache_state_report:
        bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) cache_state_emit_deferred_for_consumer:
        bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) trim_requested: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) trim_deferred_for_consumer:
        bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) trim_deferred_for_recovery:
        bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) trim_outcome:
        DemuxPacketTrimOutcome,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) timing:
        DemuxPacketAppendTiming,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop) struct DemuxPacketTrimOutcome {
    pub(in crate::player::backend::ffmpeg::playback_loop) performed: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop) steps: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop) removed_packets: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop) removed_bytes: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop) compacted_global_entries: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop) global_order_len_before: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop) global_order_len_after: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop) budget_exhausted: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop) remaining_overrun_bytes: usize,
}

impl DemuxPacketTrimOutcome {
    pub(in crate::player::backend::ffmpeg::playback_loop) fn merged(self, other: Self) -> Self {
        let other_recorded = other.performed
            || other.global_order_len_before > 0
            || other.budget_exhausted
            || other.remaining_overrun_bytes > 0;
        Self {
            performed: self.performed || other.performed,
            steps: self.steps.saturating_add(other.steps),
            removed_packets: self.removed_packets.saturating_add(other.removed_packets),
            removed_bytes: self.removed_bytes.saturating_add(other.removed_bytes),
            compacted_global_entries: self
                .compacted_global_entries
                .saturating_add(other.compacted_global_entries),
            global_order_len_before: if self.global_order_len_before == 0 {
                other.global_order_len_before
            } else {
                self.global_order_len_before
            },
            global_order_len_after: if other_recorded {
                other.global_order_len_after
            } else {
                self.global_order_len_after
            },
            budget_exhausted: self.budget_exhausted || other.budget_exhausted,
            remaining_overrun_bytes: if other_recorded {
                other.remaining_overrun_bytes
            } else {
                self.remaining_overrun_bytes
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct CachePauseRefresh {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) force_cache_state_report:
        bool,
}

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct CacheStateEmit {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) session_id:
        PlaybackSessionId,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) cache_state:
        PlaybackCacheState,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) buffered_changed:
        Option<Option<f64>>,
}

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxCacheReportSnapshot {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) session_id:
        PlaybackSessionId,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) demux: DemuxCacheState,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) paused_for_cache: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) buffering_percent:
        Option<u8>,
}

impl DemuxCacheReportSnapshot {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn into_cache_state(
        self,
    ) -> PlaybackCacheState {
        PlaybackCacheState {
            demux: self.demux,
            byte: None,
            paused_for_cache: self.paused_for_cache,
            buffering_percent: self.buffering_percent,
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn cache_end(
        &self,
    ) -> Option<f64> {
        self.demux.cache_end
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn seekable_ranges(
        &self,
    ) -> &Vec<PlaybackCacheTimeRange> {
        &self.demux.seekable_ranges
    }
}

#[derive(Clone, Copy)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxInputRateSample {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) at: Instant,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) bytes: usize,
}

#[derive(Clone, Copy)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxSeekRequest {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) position_seconds: f64,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) session_id:
        PlaybackSessionId,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) seek_generation: u64,
}

#[derive(Clone, Copy, Default)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxSelectedStreams {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) audio_stream:
        Option<StreamInfo>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) subtitle_stream:
        Option<StreamInfo>,
}
