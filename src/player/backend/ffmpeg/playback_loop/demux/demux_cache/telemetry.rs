pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use super::{
    DEMUX_PACKET_APPEND_TIMING_LOG_AFTER, DemuxPacketCacheState, nsecs_to_seconds,
};
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use super::{
    DemuxPacketAppendOutcome, DemuxPacketAppendTiming, DemuxPacketTrimOutcome,
};

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) mod types {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use super::{
        DemuxPacketAppendOutcome, DemuxPacketAppendTiming,
    };
}

#[path = "telemetry/diagnostics.rs"]
mod diagnostics;
#[path = "telemetry/snapshot.rs"]
mod snapshot;

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use diagnostics::{
    demux_cache_blocked_on, log_demux_packet_append_timing,
};
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use snapshot::DemuxPacketCacheMonitorSnapshot;
pub(in crate::player::backend::ffmpeg) use snapshot::DemuxReaderWatermark;
pub(in crate::player::backend::ffmpeg::playback_loop) use snapshot::{
    DemuxPacketCacheReadTiming, DemuxPacketQueueSnapshot, DemuxStreamPacketQueueSnapshot,
};
