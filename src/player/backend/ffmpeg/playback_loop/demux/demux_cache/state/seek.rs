use std::{collections::BTreeMap, os::raw::c_int};

use super::{
    DemuxCachedRange, DemuxCachedSeekHit, DemuxPacketCacheState, DemuxPacketRangeView,
    DemuxSeekRequest, PacketId, PlaybackSeekMode, PlaybackSessionId, RangeId, nsecs_to_seconds,
};

impl DemuxPacketCacheState {
    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn seek_cached(
        &mut self,
        target_nsecs: u64,
        session_id: PlaybackSessionId,
    ) -> Option<f64> {
        self.seek_cached_with_generation(target_nsecs, PlaybackSeekMode::Precise, session_id, 0)
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn seek_cached_fast(
        &mut self,
        target_nsecs: u64,
        session_id: PlaybackSessionId,
    ) -> Option<f64> {
        self.seek_cached_with_generation(target_nsecs, PlaybackSeekMode::Fast, session_id, 0)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn seek_cached_with_generation(
        &mut self,
        target_nsecs: u64,
        mode: PlaybackSeekMode,
        session_id: PlaybackSessionId,
        seek_generation: u64,
    ) -> Option<f64> {
        let current_hit = self.seek_cached_in_range(self.read_range(), target_nsecs, mode);
        if let Some(hit) = current_hit {
            let buffered_until_nsecs = hit.buffered_until_nsecs;
            self.reader_heads = hit.reader_heads;
            self.refresh_reader_tracking();
            self.reader_nsecs = target_nsecs;
            self.session_id = session_id;
            self.seek_request = None;
            self.seeking = false;
            self.trim_to_limit();
            self.generation = self.generation.saturating_add(1);
            self.cached_seeks = self.cached_seeks.saturating_add(1);
            self.refresh_readahead_hysteresis();
            return Some(nsecs_to_seconds(buffered_until_nsecs));
        }

        let detached_append_range_id = self.detached_append_range_id();
        let detached_hit = detached_append_range_id.and_then(|range_id| {
            self.ranges.get(&range_id).and_then(|range| {
                self.seek_cached_in_range(range, target_nsecs, mode)
                    .map(|hit| (range_id, hit))
            })
        });
        if let Some((range_id, hit)) = detached_hit {
            let buffered_until_nsecs = hit.buffered_until_nsecs;
            self.preserve_current_range();
            self.activate_range_for_read_with_heads(range_id, hit.reader_heads);
            self.reader_nsecs = target_nsecs;
            self.session_id = session_id;
            self.seek_request = None;
            self.seeking = false;
            self.trim_to_limit();
            self.generation = self.generation.saturating_add(1);
            self.cached_seeks = self.cached_seeks.saturating_add(1);
            self.refresh_readahead_hysteresis();
            return Some(nsecs_to_seconds(buffered_until_nsecs));
        }

        let hit = self
            .ranges
            .iter()
            .filter(|(range_id, _)| **range_id != self.read_range_id)
            .filter(|(range_id, _)| Some(**range_id) != detached_append_range_id)
            .find_map(|(range_id, range)| {
                self.seek_cached_in_range(range, target_nsecs, mode)
                    .map(|hit| (*range_id, hit))
            });
        let (range_id, hit) = hit?;
        let buffered_until_nsecs = hit.buffered_until_nsecs;
        self.preserve_current_range();
        self.activate_range_for_read_with_heads(range_id, hit.reader_heads);
        self.reader_nsecs = target_nsecs;
        self.session_id = session_id;
        self.seek_request = None;
        self.seeking = false;
        if !self.read_range_eof() {
            self.queue_resume_seek_after_cached_range(buffered_until_nsecs, seek_generation);
        }
        self.trim_to_limit();
        self.generation = self.generation.saturating_add(1);
        self.cached_seeks = self.cached_seeks.saturating_add(1);
        self.refresh_readahead_hysteresis();
        Some(nsecs_to_seconds(buffered_until_nsecs))
    }

    fn seek_cached_in_range(
        &self,
        range: &DemuxCachedRange,
        target_nsecs: u64,
        mode: PlaybackSeekMode,
    ) -> Option<DemuxCachedSeekHit> {
        let eager_seekable_until = self.range_cached_seekable_until(range, target_nsecs)?;
        let cached_seek_preroll_nsecs = match mode {
            PlaybackSeekMode::Precise => self.cached_seek_preroll_nsecs,
            PlaybackSeekMode::Fast => 0,
        };
        let mut hit = Self::seek_cached_in_packet_range(
            &self.packets,
            self.timeline_anchor_stream_index,
            cached_seek_preroll_nsecs,
            DemuxPacketRangeView {
                global_order: &range.global_order,
                stream_queues: &range.stream_queues,
                is_bof: range.is_bof,
                is_eof: range.is_eof,
            },
            target_nsecs,
        )?;
        hit.buffered_until_nsecs = hit.buffered_until_nsecs.min(eager_seekable_until);
        Some(hit)
    }

    fn range_cached_seekable_until(
        &self,
        range: &DemuxCachedRange,
        target_nsecs: u64,
    ) -> Option<u64> {
        let ranges = self.range_seekable_timeline_ranges(range);
        let first = ranges.first().copied()?;
        let last = ranges.last().copied()?;

        if target_nsecs < first.0 {
            return range.is_bof.then_some(first.1);
        }
        if target_nsecs > last.1 {
            return range.is_eof.then_some(last.1);
        }
        ranges
            .into_iter()
            .find(|(start, end)| *start <= target_nsecs && target_nsecs <= *end)
            .map(|(_, end)| end)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn activate_range_for_read(
        &mut self,
        range_id: RangeId,
        read_index: usize,
    ) {
        self.read_range_id = range_id;
        self.append_range_id = range_id;
        let range_len = self.read_range().global_order.len();
        self.read_range_mut().last_used_generation = self.generation;
        self.read_index = read_index.min(range_len);
        self.reset_reader_heads_for_read_index();
    }

    fn activate_range_for_read_with_heads(
        &mut self,
        range_id: RangeId,
        reader_heads: BTreeMap<c_int, PacketId>,
    ) {
        self.read_range_id = range_id;
        self.append_range_id = range_id;
        self.read_range_mut().last_used_generation = self.generation;
        self.reader_heads = reader_heads;
        self.refresh_reader_tracking();
    }

    fn queue_resume_seek_after_cached_range(
        &mut self,
        buffered_until_nsecs: u64,
        seek_generation: u64,
    ) {
        self.seek_request = Some(DemuxSeekRequest {
            position_seconds: nsecs_to_seconds(buffered_until_nsecs),
            session_id: self.session_id,
            seek_generation,
        });
        self.demux_position_detached = false;
        self.resume_append_skip_until_nsecs = Some(buffered_until_nsecs);
        self.start_detached_append_range();
        self.seeking = true;
        self.low_level_seeks = self.low_level_seeks.saturating_add(1);
        self.demux_ts_nsecs = None;
        self.read_range_mut().is_eof = false;
        self.hysteresis_active = false;
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn request_seek(
        &mut self,
        position_seconds: f64,
        session_id: PlaybackSessionId,
        seek_generation: u64,
        target_nsecs: u64,
    ) {
        tracing::debug!(
            ?session_id,
            position_seconds,
            seek_generation,
            target_nsecs,
            packet_count = self.packets.len(),
            global_packet_count = self.read_range().global_order.len(),
            archived_range_count = self.ranges.len(),
            read_index = self.read_index,
            previous_generation = self.generation,
            "preserving FFmpeg demux packet cache range for low-level seek"
        );
        self.preserve_current_range();
        self.preserve_detached_append_range();
        self.clear_reader_tracking();
        self.cache_buffering_percent = None;
        self.reader_nsecs = target_nsecs;
        self.session_id = session_id;
        self.seek_request = Some(DemuxSeekRequest {
            position_seconds,
            session_id,
            seek_generation,
        });
        self.demux_position_detached = false;
        self.resume_append_skip_until_nsecs = None;
        self.start_new_current_range(target_nsecs == 0);
        self.seeking = true;
        self.low_level_seeks = self.low_level_seeks.saturating_add(1);
        self.demux_ts_nsecs = None;
        self.generation = self.generation.saturating_add(1);
        self.hysteresis_active = false;
        self.trim_to_limit();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn request_continuation_seek(
        &mut self,
        seek_generation: u64,
    ) {
        let position_seconds = nsecs_to_seconds(self.reader_nsecs);
        let session_id = self.session_id;
        self.preserve_current_range();
        self.preserve_detached_append_range();
        self.clear_reader_tracking();
        self.cache_buffering_percent = None;
        self.seek_request = Some(DemuxSeekRequest {
            position_seconds,
            session_id,
            seek_generation,
        });
        self.demux_position_detached = false;
        self.resume_append_skip_until_nsecs = None;
        self.start_new_current_range(false);
        self.seeking = true;
        self.low_level_seeks = self.low_level_seeks.saturating_add(1);
        self.demux_ts_nsecs = None;
        self.generation = self.generation.saturating_add(1);
        self.hysteresis_active = false;
        self.trim_to_limit();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn take_seek_request(
        &mut self,
    ) -> Option<DemuxSeekRequest> {
        self.seek_request.take()
    }
}
