use std::{os::raw::c_int, sync::atomic::AtomicU64, time::Duration};

pub(super) const FALLBACK_AUDIO_OUTPUT_CHANNELS: c_int = 2;
pub(super) const POSITION_QUERY_INTERVAL: Duration = Duration::from_millis(250);
pub(super) const DEFAULT_VIDEO_FRAME_DURATION_NSECS: u64 = 1_000_000_000 / 24;
pub(super) const SCHEDULER_POLL_INTERVAL: Duration = Duration::from_millis(5);
pub(super) const RPU_MATCH_TOLERANCE: Duration = Duration::from_millis(60);
pub(super) const RPU_QUEUE_CAPACITY: usize = 2048;
pub(super) const AUDIO_BUFFER_SECONDS: usize = 4;
pub(super) const AUDIO_DECODE_QUEUE_LIMIT_DURATION: Duration = Duration::from_secs(1);
pub(super) const AUDIO_OUTPUT_QUEUE_LIMIT_DURATION: Duration = AUDIO_DECODE_QUEUE_LIMIT_DURATION;
pub(super) const AUDIO_QUEUE_WAIT_LOG_AFTER: Duration = Duration::from_millis(50);
pub(super) const AUDIO_CALLBACK_GAP_LOG_AFTER: Duration = Duration::from_millis(50);
pub(super) const AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION: Duration = Duration::from_millis(250);
pub(super) const AUDIO_VIDEO_QUEUE_LIMIT_DURATION: Duration = Duration::from_millis(1500);
pub(super) const AUDIO_VIDEO_QUEUE_TARGET_DURATION: Duration = Duration::from_millis(600);
pub(super) const DECODED_VIDEO_QUEUE_LIMIT_FRAMES: usize = 48;
pub(super) const DECODED_VIDEO_QUEUE_TARGET_FRAMES: usize = 36;
pub(super) const VULKAN_DECODED_VIDEO_QUEUE_LIMIT_FRAMES: usize = 48;
pub(super) const VULKAN_DECODED_VIDEO_QUEUE_TARGET_FRAMES: usize = 36;
pub(super) const VULKAN_VIDEO_OUTPUT_RESOURCE_PRESSURE_FRAMES: usize = 20;
pub(super) const AUDIO_CLOCK_VIDEO_PRESENT_LEAD: Duration = Duration::from_millis(15);
pub(super) const AUDIO_OUTPUT_DELAY_LIMIT: Duration = Duration::from_millis(350);
pub(super) const VULKAN_AUDIO_VIDEO_QUEUE_LIMIT_DURATION: Duration = Duration::from_millis(1500);
pub(super) const VULKAN_AUDIO_VIDEO_QUEUE_TARGET_DURATION: Duration = Duration::from_millis(600);
pub(super) const PENDING_START_AUDIO_BACKPRESSURE_DURATION: Duration = Duration::from_millis(1500);
pub(super) const PLAYING_PENDING_AUDIO_FORCE_RECOVERY_DURATION: Duration = Duration::from_secs(2);
pub(super) const PLAYING_PENDING_AUDIO_HARD_RESET_DURATION: Duration = Duration::from_secs(5);
pub(super) const PGS_SUBTITLE_VIDEO_QUEUE_LIMIT_DURATION: Duration = Duration::from_secs(4);
pub(super) const PGS_SUBTITLE_VIDEO_QUEUE_TARGET_DURATION: Duration = Duration::from_secs(3);
pub(super) const VIDEO_OUTPUT_REBUFFER_ENTER_AFTER: Duration = Duration::from_millis(250);
pub(super) const VIDEO_OUTPUT_UNDERRUN_FAST_RECOVERY_AFTER: Duration = Duration::from_millis(150);
pub(super) const VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION: Duration = Duration::from_millis(250);
pub(super) const VIDEO_OUTPUT_REBUFFER_RESUME_DURATION: Duration = Duration::from_millis(1000);
pub(super) const VIDEO_OUTPUT_REBUFFER_MIN_STABLE_RESUME_DURATION: Duration =
    Duration::from_millis(1000);
pub(super) const VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER: Duration =
    Duration::from_millis(1000);
pub(super) const VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER: Duration =
    Duration::from_millis(2000);
pub(super) const VIDEO_OUTPUT_START_PREBUFFER_DURATION: Duration = Duration::from_millis(250);
pub(super) const VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER: Duration = Duration::from_millis(800);
pub(super) const VIDEO_DECODE_SKIP_NONREF_LOW_WATER_DURATION: Duration = Duration::from_millis(900);
#[cfg(test)]
pub(super) const VIDEO_OUTPUT_REBUFFER_RESUME_FRAMES: usize = 20;
#[cfg(test)]
pub(super) const VIDEO_OUTPUT_START_PREBUFFER_FRAMES: usize = 20;
pub(super) const AUDIO_OUTPUT_VIDEO_LEAD_DURATION: Duration = Duration::from_millis(500);
pub(super) const AUDIO_VIDEO_REBUFFER_DRIFT_RESET_THRESHOLD: Duration = Duration::from_millis(500);
pub(super) const PENDING_AUDIO_CONTINUITY_TOLERANCE: Duration = Duration::from_millis(5);
pub(super) const DECODE_PACKET_SLOW_LOG_AFTER: Duration = Duration::from_millis(20);
pub(super) const PLAYBACK_COORDINATOR_TICK_TIMING_LOG_AFTER: Duration = Duration::from_millis(5);
pub(super) const PLAYBACK_COORDINATOR_STAGE_TIMING_LOG_AFTER: Duration = Duration::from_millis(3);
pub(super) const OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER: Duration = Duration::from_millis(3);
pub(super) const DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER: Duration =
    Duration::from_millis(3);
pub(super) const AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER: Duration = Duration::from_millis(3);
pub(super) const WORKER_CHANNEL_RECV_WAIT_LOG_AFTER: Duration = Duration::from_millis(50);
pub(super) const WORKER_CHANNEL_SEND_WAIT_LOG_AFTER: Duration = Duration::from_millis(5);
pub(super) const DEMUX_CACHE_LOCK_TIMING_LOG_AFTER: Duration = Duration::from_millis(1);
#[cfg(test)]
pub(super) const DEMUX_PACKET_CACHE_MEMORY_BYTES: usize = 256 * 1024 * 1024;
pub(super) const DEMUX_PACKET_CACHE_WAIT_INTERVAL: Duration = Duration::from_millis(10);
pub(super) const DEMUX_PACKET_CACHE_LOCK_WAIT: Duration = Duration::from_millis(5);
pub(super) const DEMUX_READ_WAIT_LOG_AFTER: Duration = Duration::from_millis(50);
pub(super) const DEMUX_PUMP_TIMING_LOG_INTERVAL: Duration = Duration::from_millis(250);
pub(super) const DEMUX_PACKET_CACHE_STALL_LOG_AFTER: Duration = Duration::from_millis(500);
pub(super) const DEMUX_PACKET_CACHE_STALL_LOG_INTERVAL: Duration = Duration::from_secs(1);
pub(super) const DEMUX_PACKET_CACHE_PREFETCH_PAUSE_LOG_AFTER: Duration = Duration::from_millis(500);
pub(super) const DEMUX_PACKET_CACHE_PREFETCH_PAUSE_LOG_INTERVAL: Duration = Duration::from_secs(5);
pub(super) const LATE_VIDEO_DROP_TOLERANCE: Duration = Duration::from_millis(75);
pub(super) const DEFAULT_PLAYBACK_VOLUME: f32 = 1.0;
pub(super) const PLAYBACK_VOLUME_SCALE: u32 = 10_000;
#[cfg(test)]
pub(super) const HTTP_RING_CACHE_CAPACITY: usize = 500 * 1024 * 1024;
pub(super) const HTTP_CACHE_CHUNK_SIZE: usize = 1024 * 1024;
#[cfg(test)]
pub(super) const HTTP_CACHE_RANGE_REQUEST_BYTES: u64 = 32 * 1024 * 1024;
pub(super) const HTTP_CACHE_SIDE_DOWNLOAD_WORKERS: usize = 2;
pub(super) const HTTP_CACHE_NEXT_RANGE_PREFETCH_NUMERATOR: u64 = 1;
pub(super) const HTTP_CACHE_NEXT_RANGE_PREFETCH_DENOMINATOR: u64 = 2;
pub(super) const HTTP_CACHE_WAIT_INTERVAL: Duration = Duration::from_millis(50);
pub(super) const HTTP_CACHE_STALL_LOG_AFTER: Duration = Duration::from_millis(500);
pub(super) const HTTP_CACHE_STALL_LOG_INTERVAL: Duration = Duration::from_secs(1);
pub(super) const HTTP_CACHE_PREFETCH_PAUSE_LOG_AFTER: Duration = Duration::from_millis(500);
pub(super) const HTTP_CACHE_PREFETCH_PAUSE_LOG_INTERVAL: Duration = Duration::from_secs(5);
pub(super) const HTTP_CACHE_MAX_READ_CHUNK_BYTES: usize = 128 * 1024;
pub(super) const HTTP_CACHE_PARTIAL_READ_MIN_BYTES: usize = 64 * 1024;
pub(super) const HTTP_CACHE_CONTENT_LEN_WAIT: Duration = Duration::from_secs(1);
pub(super) const HTTP_CACHE_RANGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES: u64 = 2 * 1024 * 1024;
pub(super) const HTTP_CACHE_SMALL_RANGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const HTTP_CACHE_PROGRESS_REPORT_THRESHOLD: f64 = 0.001;
#[cfg(test)]
pub(super) const HTTP_CACHE_PROBE_READ_WAIT: Duration = Duration::from_millis(250);
#[cfg(test)]
pub(super) const HTTP_CACHE_DEFAULT_READAHEAD_SECONDS: f64 = 120.0;
#[cfg(test)]
pub(super) const HTTP_CACHE_DEFAULT_HYSTERESIS_SECONDS: f64 = 10.0;
pub(super) const FFMPEG_AVIO_BUFFER_SIZE: c_int = 1024 * 1024;
pub(super) const FFMPEG_FAST_PROBE_SIZE: usize = 1024 * 1024;
pub(super) const FFMPEG_FAST_ANALYZE_DURATION_US: u64 = 1_000_000;
pub(super) const FFMPEG_SUBTITLE_PROBE_SIZE: usize = 64 * 1024 * 1024;
pub(super) const FFMPEG_SUBTITLE_ANALYZE_DURATION_US: u64 = 30_000_000;

pub(super) static FFMPEG_FRAME_COUNT: AtomicU64 = AtomicU64::new(0);
