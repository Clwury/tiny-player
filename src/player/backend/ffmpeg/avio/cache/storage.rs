use std::{
    env,
    fs::OpenOptions,
    os::unix::fs::FileExt,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::player::backend::CacheUnlinkPolicy;

use super::{HttpCachedByteRange, HttpDiskCache};

impl HttpDiskCache {
    pub(in crate::player::backend::ffmpeg::avio::cache) fn new(
        max_bytes: u64,
        configured_dir: Option<PathBuf>,
        unlink_files: CacheUnlinkPolicy,
    ) -> Option<Self> {
        let dir = configured_dir
            .or_else(|| env::var("TINY_HTTP_CACHE_DIR").ok().map(PathBuf::from))
            .unwrap_or_else(env::temp_dir);
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

        Some(Self {
            file,
            path,
            ranges: Vec::new(),
            max_bytes,
            access_generation: 0,
            unlink_on_drop,
        })
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn write_at(
        &mut self,
        offset: u64,
        data: &[u8],
    ) -> std::io::Result<()> {
        let mut written = 0;
        while written < data.len() {
            let written_now = self
                .file
                .write_at(&data[written..], offset.saturating_add(written as u64))?;
            if written_now == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "disk cache write returned zero bytes",
                ));
            }
            written += written_now;
        }
        self.add_range(offset, offset.saturating_add(data.len() as u64));
        self.trim_to_limit();
        Ok(())
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn read_at(
        &mut self,
        offset: u64,
        output: &mut [u8],
    ) -> Option<usize> {
        let range_index = self.range_index_containing(offset)?;
        let range = self.ranges[range_index];
        let len = output.len().min(usize::try_from(range.end - offset).ok()?);
        if len == 0 {
            return None;
        }
        let read = self
            .file
            .read_at(&mut output[..len], offset)
            .ok()
            .filter(|read| *read > 0)?;
        let generation = self.next_access_generation();
        if let Some(range) = self.ranges.get_mut(range_index) {
            range.last_used_generation = generation;
        }
        Some(read)
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn add_range(
        &mut self,
        start: u64,
        end: u64,
    ) {
        if end <= start {
            return;
        }
        let generation = self.next_access_generation();
        self.ranges.push(HttpCachedByteRange {
            start,
            end,
            last_used_generation: generation,
        });
        self.ranges.sort_by_key(|range| range.start);

        let mut merged: Vec<HttpCachedByteRange> = Vec::with_capacity(self.ranges.len());
        for range in self.ranges.drain(..) {
            if let Some(last) = merged.last_mut()
                && range.start <= last.end
            {
                last.end = last.end.max(range.end);
                last.last_used_generation =
                    last.last_used_generation.max(range.last_used_generation);
                continue;
            }
            merged.push(range);
        }
        self.ranges = merged;
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn trim_to_limit(&mut self) {
        loop {
            let cached_bytes = self.cached_bytes();
            if cached_bytes <= self.max_bytes {
                break;
            }
            let Some(range_index) = self
                .ranges
                .iter()
                .enumerate()
                .min_by_key(|(_, range)| range.last_used_generation)
                .map(|(index, _)| index)
            else {
                break;
            };
            let overflow = cached_bytes.saturating_sub(self.max_bytes);
            let Some(range) = self.ranges.get_mut(range_index) else {
                break;
            };
            let trim = overflow.min(range.end.saturating_sub(range.start));
            range.start = range.start.saturating_add(trim);
            if range.start >= range.end {
                self.ranges.remove(range_index);
            }
        }
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
