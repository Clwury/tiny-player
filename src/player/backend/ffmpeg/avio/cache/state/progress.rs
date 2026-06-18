use crate::player::backend::PlaybackCacheByteRange;

#[cfg(test)]
use super::HttpCacheRangeKind;
use super::{HttpPlaybackBufferRange, HttpRingCacheState};

impl HttpRingCacheState {
    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn stream_buffer_progress(
        &self,
    ) -> Option<PlaybackCacheByteRange> {
        let content_len = self.content_len?;
        if content_len == 0 {
            return None;
        }

        let mut start_offset = None;
        let mut end_offset = None;
        for range in self
            .retained_ranges
            .iter()
            .filter(|range| range.range_kind == HttpCacheRangeKind::Playback)
        {
            extend_buffer_progress_range(
                &mut start_offset,
                &mut end_offset,
                range.base_offset,
                range.next_offset,
                content_len,
            );
        }
        if self.active_range_kind == HttpCacheRangeKind::Playback {
            extend_buffer_progress_range(
                &mut start_offset,
                &mut end_offset,
                self.base_offset,
                self.next_offset,
                content_len,
            );
        }
        let start_offset = start_offset?;
        let end_offset = end_offset?.max(start_offset);
        let content_len = content_len as f64;
        Some(PlaybackCacheByteRange {
            start_fraction: start_offset as f64 / content_len,
            end_fraction: end_offset as f64 / content_len,
        })
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn playback_buffer_range(
        &self,
    ) -> Option<HttpPlaybackBufferRange> {
        let content_len = self.content_len?;
        if content_len == 0 {
            return None;
        }

        match self.active_range_kind {
            HttpCacheRangeKind::Playback => {
                playback_buffer_range(self.base_offset, self.next_offset, content_len)
            }
            HttpCacheRangeKind::TailMetadataProbe => {
                let range = self
                    .retained_ranges
                    .iter()
                    .rev()
                    .find(|range| range.range_kind == HttpCacheRangeKind::Playback)?;
                playback_buffer_range(range.base_offset, range.next_offset, content_len)
            }
        }
    }
}

#[cfg(test)]
fn extend_buffer_progress_range(
    start_offset: &mut Option<u64>,
    end_offset: &mut Option<u64>,
    base_offset: u64,
    next_offset: u64,
    content_len: u64,
) {
    let range_start = base_offset.min(content_len);
    let range_end = next_offset.min(content_len).max(range_start);
    if range_end <= range_start {
        return;
    }
    *start_offset = Some(start_offset.map_or(range_start, |start| start.min(range_start)));
    *end_offset = Some(end_offset.map_or(range_end, |end| end.max(range_end)));
}

pub(in crate::player::backend::ffmpeg::avio::cache) fn playback_buffer_range(
    base_offset: u64,
    next_offset: u64,
    content_len: u64,
) -> Option<HttpPlaybackBufferRange> {
    let range_start = base_offset.min(content_len);
    let range_end = next_offset.min(content_len).max(range_start);
    if range_end <= range_start {
        return None;
    }

    Some(HttpPlaybackBufferRange {
        start_offset: range_start,
        end_offset: range_end,
        content_len,
    })
}

pub(in crate::player::backend::ffmpeg::avio::cache) fn merge_playback_buffer_ranges(
    mut ranges: Vec<HttpPlaybackBufferRange>,
) -> Vec<HttpPlaybackBufferRange> {
    ranges.sort_by_key(|range| range.start_offset);
    let mut merged: Vec<HttpPlaybackBufferRange> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(last) = merged.last_mut()
            && range.start_offset <= last.end_offset
        {
            last.end_offset = last.end_offset.max(range.end_offset);
            continue;
        }
        merged.push(range);
    }
    merged
}

pub(in crate::player::backend::ffmpeg::avio::cache) fn merged_cached_byte_len(
    mut ranges: Vec<(u64, u64)>,
) -> u64 {
    ranges.retain(|(start, end)| end > start);
    ranges.sort_by_key(|(start, _)| *start);

    let mut total = 0u64;
    let mut current: Option<(u64, u64)> = None;
    for (start, end) in ranges {
        match current {
            Some((current_start, current_end)) if start <= current_end => {
                current = Some((current_start, current_end.max(end)));
            }
            Some((current_start, current_end)) => {
                total = total.saturating_add(current_end.saturating_sub(current_start));
                current = Some((start, end));
            }
            None => current = Some((start, end)),
        }
    }
    if let Some((start, end)) = current {
        total = total.saturating_add(end.saturating_sub(start));
    }
    total
}

impl From<HttpPlaybackBufferRange> for PlaybackCacheByteRange {
    fn from(range: HttpPlaybackBufferRange) -> Self {
        let content_len = range.content_len as f64;
        Self {
            start_fraction: range.start_offset as f64 / content_len,
            end_fraction: range.end_offset as f64 / content_len,
        }
    }
}
