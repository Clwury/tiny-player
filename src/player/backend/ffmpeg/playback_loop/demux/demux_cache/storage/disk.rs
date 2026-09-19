use std::{
    env,
    fs::{File, OpenOptions},
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
use super::AvPacket;
use super::{CacheUnlinkPolicy, PlaybackCacheConfig};
use crate::app_metadata::default_playback_cache_dir;
use crate::player::backend::ffmpeg::disk_cache::{BoundedDiskFile, DiskBlock, read_at};

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxPacketDiskCache {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) path: PathBuf,
    storage: BoundedDiskFile,
    unlink_on_drop: bool,
}

impl DemuxPacketDiskCache {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn from_config(
        config: &PlaybackCacheConfig,
    ) -> Option<Self> {
        if !config.disk_cache && !demux_packet_disk_cache_enabled() {
            return None;
        }
        let budget = config.effective_disk_cache_budgets().1;
        let max_bytes = env::var("TINY_DEMUX_PACKET_CACHE_BYTES")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(budget)
            .min(budget);
        Self::new(max_bytes, config.cache_dir.clone(), config.unlink_files)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn new(
        max_bytes: u64,
        configured_dir: Option<PathBuf>,
        unlink_files: CacheUnlinkPolicy,
    ) -> Option<Self> {
        let dir = configured_dir
            .or_else(|| {
                env::var("TINY_DEMUX_PACKET_CACHE_DIR")
                    .ok()
                    .map(PathBuf::from)
            })
            .unwrap_or_else(default_playback_cache_dir);
        if let Err(error) = std::fs::create_dir_all(&dir) {
            tracing::warn!(%error, path = %dir.display(), "failed to create demux packet cache directory");
            return None;
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let path = dir.join(format!(
            "tiny-demux-packet-cache-{}-{stamp}.tmp",
            std::process::id()
        ));
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "failed to create demux packet cache file");
                return None;
            }
        };
        let mut unlink_on_drop = matches!(unlink_files, CacheUnlinkPolicy::WhenDone);
        if matches!(unlink_files, CacheUnlinkPolicy::Immediate) {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "failed to immediately unlink demux packet cache file");
                    unlink_on_drop = true;
                }
            }
        }
        let file = Arc::new(file);
        Some(Self {
            storage: BoundedDiskFile::new(Arc::clone(&file), max_bytes),
            path,
            unlink_on_drop,
        })
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn write_packet(
        &mut self,
        data: &[u8],
    ) -> std::result::Result<Arc<DiskBlock>, String> {
        let block = self
            .reserve_packet(data.len())
            .ok_or("FFmpeg demux packet disk cache 已满")?;
        block.write(data).map_err(|error| error.to_string())?;
        Ok(block)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn reserve_packet(
        &self,
        len: usize,
    ) -> Option<Arc<DiskBlock>> {
        self.storage.reserve(len)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn file_bytes(&self) -> u64 {
        self.storage.file_len()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn limit(&self) -> u64 {
        self.storage.limit()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn set_limit(
        &self,
        config: &PlaybackCacheConfig,
    ) {
        let budget = config.effective_disk_cache_budgets().1;
        let limit = env::var("TINY_DEMUX_PACKET_CACHE_BYTES")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(budget)
            .min(budget);
        self.storage.set_limit(limit);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn maintain_file_size(
        &self,
    ) {
        self.storage.maintain_file_size();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn accepts(
        &self,
        block: &DiskBlock,
    ) -> bool {
        self.storage.accepts(block)
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn read_packet(
        &self,
        block: Arc<DiskBlock>,
        len: usize,
        props: &AvPacket,
    ) -> std::result::Result<AvPacket, String> {
        let data = read_demux_packet_disk_payload(&block.file, block.offset, len)?;
        AvPacket::from_data_and_props(&data, props)
    }
}

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn read_demux_packet_disk_payload(
    file: &File,
    offset: u64,
    len: usize,
) -> std::result::Result<Vec<u8>, String> {
    let mut data = vec![0; len];
    let mut read = 0;
    while read < data.len() {
        let read_now = read_at(file, &mut data[read..], offset.saturating_add(read as u64))
            .map_err(|error| format!("读取 FFmpeg demux packet disk cache 失败：{error}"))?;
        if read_now == 0 {
            return Err("读取 FFmpeg demux packet disk cache 返回 0 字节".to_string());
        }
        read += read_now;
    }
    Ok(data)
}

impl Drop for DemuxPacketDiskCache {
    fn drop(&mut self) {
        if !self.unlink_on_drop {
            return;
        }
        if let Err(error) = std::fs::remove_file(&self.path) {
            tracing::debug!(%error, path = %self.path.display(), "failed to remove demux packet cache file");
        }
    }
}

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn demux_packet_disk_cache_enabled()
-> bool {
    env::var("TINY_DEMUX_PACKET_CACHE_ON_DISK")
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}
