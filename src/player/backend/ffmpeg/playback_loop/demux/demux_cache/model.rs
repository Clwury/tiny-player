pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use super::{
    AvPacket, AvPacketReadDiagnostic, AvPacketStorageKind, BackendEvent, BufferedReporter,
    DEFAULT_VIDEO_FRAME_DURATION_NSECS, DemuxPacketCacheReadTiming, DemuxPacketDiskCache,
    FormatContext, PlaybackCacheConfig, PlaybackCacheState, PlaybackSessionId, StreamInfo,
    TimestampMapper, VideoRecoveryPointKind, packet_duration_nsecs, packet_is_audio_recovery_point,
    packet_is_video_seek_point, packet_video_recovery_point_kind, read_demux_packet_disk_payload,
    seconds_to_nsecs,
};

#[path = "model/packet.rs"]
mod packet;
#[path = "model/range.rs"]
mod range;
#[path = "model/seekable.rs"]
mod seekable;
#[path = "model/stream_window.rs"]
mod stream_window;
#[path = "model/timeline.rs"]
mod timeline;
#[path = "model/types.rs"]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) mod types;

#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use packet::CachedDemuxPacketPayload;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use packet::{
    CachedDemuxPacket, CachedDemuxPacketRecovery, DemuxPacketReadSource,
};
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use range::{
    ArchivedStreamPruneCandidate, DemuxCachedRange, DemuxCachedSeekHit, DemuxPacketRangeView,
    SeekableTimelineSummary,
};
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use seekable::ordered_duration_seconds;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use stream_window::{
    StreamCacheRangeState, StreamForwardState, StreamForwardWindow,
};
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use timeline::DemuxPacketTimeline;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use types::{
    CachePauseRefresh, CacheStateEmit, DemuxCacheLockWait, DemuxCacheReportSnapshot,
    DemuxInputRateSample, DemuxPacketAppendOutcome, DemuxPacketAppendTiming,
    DemuxPacketCacheThreadInput, DemuxPacketTrimOutcome, DemuxSeekRequest, DemuxSelectedStreams,
    PacketId, RangeId,
};
pub(in crate::player::backend::ffmpeg::playback_loop) use types::{
    DemuxCachedSeekInfo, DemuxPacketCacheInput, DemuxReadResult, DemuxSeekResult,
    DemuxStreamReaderRealignResult,
};
