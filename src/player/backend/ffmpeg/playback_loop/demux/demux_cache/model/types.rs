use std::time::{Duration, Instant};

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop) enum DemuxSeekResult {
    Cached,
    Requested,
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
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) notify: Duration,
}

#[derive(Clone, Copy, Debug, Default)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxPacketAppendOutcome {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) appended: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) force_cache_state_report:
        bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) timing:
        DemuxPacketAppendTiming,
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
