use std::{
    env,
    fs::OpenOptions,
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{app_metadata::default_playback_cache_dir, player::backend::CacheUnlinkPolicy};

use super::{HttpCachedByteRange, HttpDiskCache};
use crate::player::backend::ffmpeg::disk_cache::{BoundedDiskFile, DiskBlock};

impl HttpDiskCache {
    pub(in crate::player::backend::ffmpeg::avio::cache) fn new(
        max_bytes: u64,
        configured_dir: Option<PathBuf>,
        unlink_files: CacheUnlinkPolicy,
    ) -> Option<Self> {
        let dir = configured_dir
            .or_else(|| env::var("TINY_HTTP_CACHE_DIR").ok().map(PathBuf::from))
            .unwrap_or_else(default_playback_cache_dir);
        if let Err(error) = std::fs::create_dir_all(&dir) {
            tracing::warn!(%error, path = %dir.display(), "failed to create HTTP disk cache directory");
            return None;
        }
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let path = dir.join(format!(
            "tiny-http-cache-{}-{stamp}.tmp",
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
                tracing::warn!(%error, path = %path.display(), "failed to create HTTP disk cache file");
                return None;
            }
        };
        let mut unlink_on_drop = matches!(unlink_files, CacheUnlinkPolicy::WhenDone);
        if matches!(unlink_files, CacheUnlinkPolicy::Immediate) {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "failed to immediately unlink HTTP disk cache file");
                    unlink_on_drop = true;
                }
            }
        }

        let file = Arc::new(file);
        Some(Self {
            storage: BoundedDiskFile::new(Arc::clone(&file), max_bytes),
            path,
            ranges: Vec::new(),
            max_bytes,
            access_generation: 0,
            unlink_on_drop,
        })
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::avio::cache) fn write_at(
        &mut self,
        offset: u64,
        data: &[u8],
    ) -> std::io::Result<()> {
        if let Some(block) = self.reserve_write(offset, data.len()) {
            block.write(data)?;
            self.add_range(offset, block);
        }
        Ok(())
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn reserve_write(
        &mut self,
        offset: u64,
        len: usize,
    ) -> Option<Arc<DiskBlock>> {
        if len == 0 || len as u64 > self.max_bytes {
            return None;
        }
        let end = offset.checked_add(len as u64)?;
        // Drop overlapping old entries before reserving space. An in-flight
        // write retains its lease and cannot be reused by another worker.
        self.ranges
            .retain(|range| range.end <= offset || range.start >= end);
        loop {
            if let Some(block) = self.storage.reserve(len) {
                return Some(block);
            }
            let victim = self
                .ranges
                .iter()
                .enumerate()
                .min_by_key(|(_, range)| range.last_used_generation)
                .map(|(index, _)| index)?;
            self.ranges.remove(victim);
        }
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn read_at(
        &mut self,
        offset: u64,
        output: &mut [u8],
    ) -> Option<usize> {
        let index = self.range_index_containing(offset)?;
        let range = &self.ranges[index];
        let read = range
            .block
            .read_at(offset - range.start, output)
            .ok()
            .filter(|read| *read > 0)?;
        let generation = self.next_access_generation();
        self.ranges[index].last_used_generation = generation;
        Some(read)
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn add_range(
        &mut self,
        start: u64,
        block: Arc<DiskBlock>,
    ) {
        if !self.storage.accepts(&block) {
            return;
        }
        let Some(end) = start.checked_add(block.len as u64) else {
            return;
        };
        self.ranges
            .retain(|range| range.end <= start || range.start >= end);
        let last_used_generation = self.next_access_generation();
        self.ranges.push(HttpCachedByteRange {
            start,
            end,
            block,
            last_used_generation,
        });
        self.ranges.sort_by_key(|range| range.start);
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn file_bytes(&self) -> u64 {
        self.storage.file_len()
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn set_limit(&mut self, max_bytes: u64) {
        self.max_bytes = max_bytes;
        self.storage.set_limit(max_bytes);
        self.ranges
            .retain(|range| self.storage.accepts(&range.block));
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn cached_bytes(&self) -> u64 {
        self.ranges
            .iter()
            .map(|range| range.end.saturating_sub(range.start))
            .sum()
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn next_access_generation(
        &mut self,
    ) -> u64 {
        self.access_generation = self.access_generation.saturating_add(1);
        self.access_generation
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn range_index_containing(
        &self,
        offset: u64,
    ) -> Option<usize> {
        self.ranges
            .iter()
            .position(|range| offset >= range.start && offset < range.end)
    }
}

impl Drop for HttpDiskCache {
    fn drop(&mut self) {
        if !self.unlink_on_drop {
            return;
        }
        if let Err(error) = std::fs::remove_file(&self.path) {
            tracing::debug!(%error, path = %self.path.display(), "failed to remove HTTP disk cache file");
        }
    }
}
