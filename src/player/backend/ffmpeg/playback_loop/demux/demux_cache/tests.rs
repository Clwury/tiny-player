#![cfg(test)]

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use super::{
    AvPacket, AvPacketStorageKind, CachedDemuxPacket, CachedDemuxPacketPayload,
    CachedDemuxPacketRecovery, CachedSeekMissReason, DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    DEMUX_PACKET_APPEND_TRIM_INTERVAL, DEMUX_PACKET_APPEND_TRIM_STEP_LIMIT,
    DEMUX_PACKET_CACHE_MAX_AUTO_HYSTERESIS, DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL,
    DEMUX_PACKET_READ_TRIM_INTERVAL, DEMUX_PACKET_READ_TRIM_MEMORY_OVERRUN_INTERVAL,
    DEMUX_PACKET_SNAPSHOT_READABLE_SCAN_LIMIT, DEMUX_PACKET_TRIM_MAX_PACKETS_PER_STEP,
    DEMUX_STREAM_PACKET_QUEUE_LIMIT, DemuxCachedSeekInfo, DemuxPacketCache,
    DemuxPacketCacheMonitorSnapshot, DemuxPacketCacheReadTiming, DemuxPacketCacheShared,
    DemuxPacketCacheState, DemuxPacketDiskCache, DemuxPacketTimeline, DemuxReadResult,
    DemuxSeekRequest, DemuxSeekResult, DemuxSelectedStreams, FfmpegControl, PacketId, StreamInfo,
    VideoRecoveryPointKind, demux_cache_blocked_on, demux_packet_cache_hysteresis_nsecs,
    demux_packet_cache_readahead_nsecs, duration_nsecs, seconds_to_nsecs,
};

#[path = "tests/cache.rs"]
mod cache;
