use std::time::Duration;

use crate::player::backend::PlaybackCacheConfig;

use super::super::{duration_nsecs, seconds_to_nsecs};

// mpv's handle_update_cache() publishes demuxer-cache-state every 250 ms while
// the demuxer is busy; the OSC redraws seekable-ranges from those updates.
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL: Duration = Duration::from_millis(250);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_STREAM_PACKET_QUEUE_LIMIT: usize = 2048;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_SUBTITLE_PACKET_QUEUE_LIMIT: usize = 4096;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_READ_SLOW_LOG_AFTER: Duration = Duration::from_millis(200);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_CACHE_MAX_AUTO_HYSTERESIS: Duration = Duration::from_secs(5);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_WOULD_BLOCK_DIAG_INTERVAL: Duration = Duration::from_millis(500);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_APPEND_TIMING_LOG_AFTER: Duration = Duration::from_millis(1);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_APPEND_MAINTENANCE_INTERVAL: usize = 16;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_APPEND_TRIM_INTERVAL: usize = 64;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_APPEND_TRIM_MAX_OVERRUN_BYTES: usize = 8 * 1024 * 1024;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_APPEND_TRIM_STEP_LIMIT: usize = 4;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_APPEND_TRIM_TIME_BUDGET: Duration = Duration::from_millis(1);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_CACHE_CONSUMER_LOCK_PRESSURE_AFTER: Duration = Duration::from_millis(20);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_CACHE_CONSUMER_PRIORITY_HOLD: Duration = Duration::from_millis(50);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_RECOVERY_YIELD_MAX_WAIT: Duration = Duration::from_millis(100);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_RECOVERY_DEMAND_DIAG_INTERVAL: Duration = Duration::from_millis(500);
// The monitor snapshot is refreshed while holding the cache mutex on every
// consumer read; callers only need "any readable packet" plus a diagnostic
// magnitude, so the readable count saturates instead of scanning the queue.
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_SNAPSHOT_READABLE_SCAN_LIMIT: usize = 256;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_READ_TRIM_INTERVAL: usize = 128;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_READ_TRIM_MEMORY_OVERRUN_INTERVAL: usize = 16;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_READ_TRIM_MEMORY_OVERRUN_BYTES: usize = 8 * 1024 * 1024;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_READ_TRIM_STEP_LIMIT: usize = 1;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_READ_TRIM_TIME_BUDGET: Duration = Duration::from_millis(1);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_TRIM_MAX_PACKETS_PER_STEP: usize = 512;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_TRIM_INLINE_GLOBAL_SCAN_LIMIT: usize = 4096;

/// Read-ahead target for the demux PACKET cache.
///
/// Seekable ranges require demuxed packets, so the default follows mpv's network
/// cache behavior: cache-active inputs target `cache_secs` and stop primarily on
/// `demuxer_max_bytes` (150 MiB by default). A non-zero packet cap remains available
/// as an explicit override; 0 disables the extra time cap.
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
/// Keep an explicit hysteresis value separate from the automatic refill band.
/// mpv can use a zero band because its demux thread does not share the packet
/// mutex with the playback consumer; tiny's producer and coordinator do. The
/// automatic band prevents wake/read/append churn, while the UI can disable it
/// by setting `automatic_hysteresis` to false.
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn demux_packet_cache_hysteresis_nsecs(
    cache_config: &PlaybackCacheConfig,
    readahead_nsecs: u64,
) -> u64 {
    let configured = seconds_to_nsecs(cache_config.demuxer_hysteresis_secs);
    if !cache_config.automatic_hysteresis {
        return configured.min(readahead_nsecs);
    }
    if configured > 0 {
        // Keep an explicitly requested band below the target. This mirrors
        // mpv's recommendation and prevents a high-bitrate byte target from
        // producing a zero-byte resume watermark in the transport cache.
        configured.min(readahead_nsecs / 2)
    } else {
        (readahead_nsecs / 3).min(duration_nsecs(DEMUX_PACKET_CACHE_MAX_AUTO_HYSTERESIS))
    }
}
