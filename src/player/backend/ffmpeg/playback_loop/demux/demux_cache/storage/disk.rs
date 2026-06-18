use std::{
    env,
    fs::{File, OpenOptions},
    os::unix::fs::FileExt,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
use super::AvPacket;
use super::{CacheUnlinkPolicy, PlaybackCacheConfig};

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxPacketDiskCache {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) file: Arc<File>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) path: PathBuf,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) next_offset: u64,
    max_bytes: u64,
    unlink_on_drop: bool,
}

impl DemuxPacketDiskCache {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn from_config(
        config: &PlaybackCacheConfig,
    ) -> Option<Self> {
        if !config.disk_cache && !demux_packet_disk_cache_enabled() {
            return None;
        }
        let max_bytes = env::var("TINY_DEMUX_PACKET_CACHE_BYTES")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(config.disk_cache_max_bytes);
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
            .unwrap_or_else(env::temp_dir);
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
        Some(Self {
            file: Arc::new(file),
            path,
            next_offset: 0,
            max_bytes,
            unlink_on_drop,
        })
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn write_packet(
        &mut self,
        data: &[u8],
    ) -> std::result::Result<u64, String> {
        let len = u64::try_from(data.len())
            .map_err(|_| "FFmpeg demux packet payload 过大".to_string())?;
        let offset = self.next_offset;
        let next = offset
            .checked_add(len)
            .ok_or_else(|| "FFmpeg demux packet disk cache offset overflow".to_string())?;
        if next > self.max_bytes {
            return Err("FFmpeg demux packet disk cache 已满".to_string());
        }
        let mut written = 0;
        while written < data.len() {
            let written_now = self
                .file
                .write_at(&data[written..], offset.saturating_add(written as u64))
                .map_err(|error| format!("写入 FFmpeg demux packet disk cache 失败：{error}"))?;
            if written_now == 0 {
                return Err("写入 FFmpeg demux packet disk cache 返回 0 字节".to_string());
            }
            written += written_now;
        }
        self.next_offset = next;
        Ok(offset)
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn read_packet(
        &self,
        offset: u64,
        len: usize,
        props: &AvPacket,
    ) -> std::result::Result<AvPacket, String> {
        let data = read_demux_packet_disk_payload(&self.file, offset, len)?;
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
        let read_now = file
            .read_at(&mut data[read..], offset.saturating_add(read as u64))
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
