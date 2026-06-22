use std::time::Duration;

use crate::player::backend::PlaybackCacheConfig;

use super::super::{duration_nsecs, seconds_to_nsecs};

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL: Duration = Duration::from_millis(250);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_STREAM_PACKET_QUEUE_LIMIT: usize = 2048;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_SUBTITLE_PACKET_QUEUE_LIMIT: usize = 4096;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_READ_SLOW_LOG_AFTER: Duration = Duration::from_millis(200);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_CACHE_MAX_AUTO_HYSTERESIS: Duration = Duration::from_secs(5);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_WOULD_BLOCK_DIAG_INTERVAL: Duration = Duration::from_millis(500);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_APPEND_TIMING_LOG_AFTER: Duration = Duration::from_millis(1);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_APPEND_MAINTENANCE_INTERVAL: usize = 16;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_APPEND_TRIM_INTERVAL: usize = 16;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_APPEND_TRIM_STEP_LIMIT: usize = 1;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_READ_TRIM_STEP_LIMIT: usize = 1;

/// Read-ahead target for the demux PACKET cache.
///
/// Seekable ranges require demuxed packets, so the default follows mpv's network
/// cache behavior: cache-active inputs target `cache_secs` and stop primarily on
/// `demuxer_max_bytes`. A non-zero packet cap is still available as an override;
/// 0 disables the extra time cap.
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn demux_packet_cache_readahead_nsecs(
    cache_config: &PlaybackCacheConfig,
    cache_active: bool,
) -> u64 {
    let target = seconds_to_nsecs(cache_config.effective_readahead_secs(cache_active));
    let max_readahead = seconds_to_nsecs(cache_config.demuxer_packet_max_readahead_secs);
    if max_readahead == 0 {
        target
    } else {
        target.min(max_readahead)
    }
}

/// Hysteresis band for the demux PACKET cache read-ahead.
///
/// The default config sets no hysteresis (mpv parity). But unlike mpv -- whose demuxer
/// thread does not share a mutex with the playback consumer -- tiny's demux producer and
/// the coordinator pump contend on a single cache mutex. With zero hysteresis the
/// producer resumes reading the instant `forward` dips below the read-ahead target, so it
/// wakes to read+append on every consumed packet, thrashing the lock against the pump
/// and starving the decoder. Inject a band (when none is configured) so the producer
/// parks between refills and the pump gets long uncontended windows to feed the decoder.
/// Cap the automatic band so larger seekable-range windows resume prefetching early.
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn demux_packet_cache_hysteresis_nsecs(
    cache_config: &PlaybackCacheConfig,
    readahead_nsecs: u64,
) -> u64 {
    let configured = seconds_to_nsecs(cache_config.demuxer_hysteresis_secs);
    if configured > 0 {
        configured
    } else {
        (readahead_nsecs / 3).min(duration_nsecs(DEMUX_PACKET_CACHE_MAX_AUTO_HYSTERESIS))
    }
}
