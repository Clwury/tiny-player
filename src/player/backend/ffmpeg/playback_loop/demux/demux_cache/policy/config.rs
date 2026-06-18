use std::time::Duration;

use crate::player::backend::PlaybackCacheConfig;

use super::super::{duration_nsecs, seconds_to_nsecs};

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_CACHE_STATE_REPORT_INTERVAL: Duration = Duration::from_millis(250);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_STREAM_PACKET_QUEUE_LIMIT: usize = 2048;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_SUBTITLE_PACKET_QUEUE_LIMIT: usize = 4096;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_READ_SLOW_LOG_AFTER: Duration = Duration::from_millis(200);
// Packet-cache read-ahead cap. Kept far below deep byte-cache buffering so the demux
// producer still parks between refills, but large enough to keep high-bitrate hardware
// decode fed through short stalls in frame preparation and transient network dips.
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_CACHE_MAX_READAHEAD: Duration = Duration::from_secs(12);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_WOULD_BLOCK_DIAG_INTERVAL: Duration = Duration::from_millis(500);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_APPEND_TIMING_LOG_AFTER: Duration = Duration::from_millis(1);
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_APPEND_MAINTENANCE_INTERVAL: usize = 16;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_APPEND_TRIM_STEP_LIMIT: usize = 2;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) const DEMUX_PACKET_READ_TRIM_STEP_LIMIT: usize = 1;

/// Read-ahead target for the demux PACKET cache.
///
/// Only a few seconds of demuxed packets are needed to keep the decoder fed; deep
/// buffering for seeking and network resilience is provided by the byte-level
/// HTTP/disk cache. This is intentionally NOT inflated to `cache_secs`: an unbounded
/// packet read-ahead makes the demux producer thread hot-loop without ever pausing,
/// monopolizing the cache mutex and starving the coordinator pump that feeds the
/// decoder (decode then collapses below realtime, causing perpetual rebuffering).
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn demux_packet_cache_readahead_nsecs(
    cache_config: &PlaybackCacheConfig,
    cache_active: bool,
) -> u64 {
    seconds_to_nsecs(cache_config.effective_readahead_secs(cache_active))
        .min(duration_nsecs(DEMUX_PACKET_CACHE_MAX_READAHEAD))
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
/// Use a narrower default band than half the target so refill starts before a slow 4K
/// decode path drains most of the demux window.
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn demux_packet_cache_hysteresis_nsecs(
    cache_config: &PlaybackCacheConfig,
    readahead_nsecs: u64,
) -> u64 {
    let configured = seconds_to_nsecs(cache_config.demuxer_hysteresis_secs);
    if configured > 0 {
        configured
    } else {
        readahead_nsecs / 3
    }
}
