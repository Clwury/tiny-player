use std::env;

#[cfg(test)]
use crate::player::backend::CacheUnlinkPolicy;
use crate::player::backend::{PlaybackCacheConfig, PlaybackCacheMode};

use super::HttpCacheConfig;
#[cfg(test)]
use super::{
    HTTP_CACHE_CHUNK_SIZE, HTTP_CACHE_DEFAULT_HYSTERESIS_SECONDS,
    HTTP_CACHE_DEFAULT_READAHEAD_SECONDS, HTTP_CACHE_RANGE_REQUEST_BYTES,
};

impl HttpCacheConfig {
    pub(in crate::player::backend::ffmpeg::avio::cache) fn from_playback_config(
        config: &PlaybackCacheConfig,
    ) -> Self {
        let config = config.clone().normalized();
        let cache_active = !matches!(config.mode, PlaybackCacheMode::Disabled);
        let configured_chunk = usize::try_from(config.http_cache_chunk_bytes)
            .unwrap_or(usize::MAX)
            .clamp(64 * 1024, 16 * 1024 * 1024);
        let configured_memory = usize::try_from(config.http_cache_max_bytes)
            .unwrap_or(usize::MAX)
            .max(configured_chunk);
        let chunk_size = env_usize("TINY_HTTP_CACHE_CHUNK_BYTES", configured_chunk)
            .clamp(64 * 1024, 16 * 1024 * 1024);
        let memory_capacity =
            env_usize("TINY_HTTP_CACHE_MEMORY_BYTES", configured_memory).max(chunk_size);
        let range_request_bytes = env_u64("TINY_HTTP_CACHE_RANGE_REQUEST_BYTES")
            .unwrap_or(config.http_cache_range_request_bytes)
            .clamp(64 * 1024, 128 * 1024 * 1024)
            .max(u64::try_from(chunk_size).unwrap_or(u64::MAX));
        Self {
            memory_capacity,
            chunk_size,
            range_request_bytes,
            readahead_seconds: env_f64(
                "TINY_HTTP_CACHE_READAHEAD_SECS",
                config.effective_readahead_secs(cache_active),
            )
            .max(1.0),
            hysteresis_seconds: env_f64(
                "TINY_HTTP_CACHE_HYSTERESIS_SECS",
                config.demuxer_hysteresis_secs,
            )
            .max(0.0),
            max_readahead_bytes: Some(
                env_u64("TINY_HTTP_CACHE_MAX_BYTES").unwrap_or(config.http_cache_max_bytes),
            ),
            disk_cache_bytes: config.disk_cache.then(|| {
                env_u64("TINY_HTTP_CACHE_DISK_BYTES").unwrap_or(config.disk_cache_max_bytes)
            }),
            cache_dir: config.cache_dir,
            unlink_files: config.unlink_files,
        }
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::avio::cache) fn for_test(
        memory_capacity: usize,
    ) -> Self {
        Self {
            memory_capacity,
            chunk_size: HTTP_CACHE_CHUNK_SIZE.min(memory_capacity.max(1)),
            range_request_bytes: HTTP_CACHE_RANGE_REQUEST_BYTES,
            readahead_seconds: HTTP_CACHE_DEFAULT_READAHEAD_SECONDS,
            hysteresis_seconds: HTTP_CACHE_DEFAULT_HYSTERESIS_SECONDS,
            max_readahead_bytes: None,
            disk_cache_bytes: None,
            cache_dir: None,
            unlink_files: CacheUnlinkPolicy::WhenDone,
        }
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn env_u64(name: &str) -> Option<u64> {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
}

fn env_f64(name: &str, default: f64) -> f64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(default)
}
