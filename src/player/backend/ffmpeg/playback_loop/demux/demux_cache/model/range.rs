use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    os::raw::c_int,
};

use super::VideoRecoveryPointKind;
use super::types::{PacketId, RangeId};

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxCachedRange {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) id: RangeId,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) global_order:
        VecDeque<PacketId>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) stream_queues:
        BTreeMap<c_int, VecDeque<PacketId>>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) stream_seek_boundaries:
        BTreeMap<c_int, VecDeque<PacketId>>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) stream_boundaries:
        BTreeMap<c_int, StreamRangeBoundary>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) report_stats:
        RefCell<RangeReportStats>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) is_bof: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) is_eof: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) last_used_generation: u64,
}

impl DemuxCachedRange {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn new(
        id: RangeId,
        is_bof: bool,
        last_used_generation: u64,
    ) -> Self {
        Self {
            id,
            global_order: VecDeque::new(),
            stream_queues: BTreeMap::new(),
            stream_seek_boundaries: BTreeMap::new(),
            stream_boundaries: BTreeMap::new(),
            report_stats: RefCell::new(RangeReportStats::default()),
            is_bof,
            is_eof: false,
            last_used_generation,
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn stream_boundary(
        &self,
        stream_index: c_int,
    ) -> StreamRangeBoundary {
        self.stream_boundaries
            .get(&stream_index)
            .copied()
            .unwrap_or_else(|| StreamRangeBoundary::new(self.is_bof, self.is_eof))
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn ensure_stream_boundary(
        &mut self,
        stream_index: c_int,
    ) -> &mut StreamRangeBoundary {
        let default_boundary = StreamRangeBoundary::new(self.is_bof, self.is_eof);
        self.stream_boundaries
            .entry(stream_index)
            .or_insert(default_boundary)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn report_bytes(
        &self,
    ) -> usize {
        self.report_stats.borrow().bytes
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn add_report_bytes(
        &self,
        bytes: usize,
    ) {
        let mut stats = self.report_stats.borrow_mut();
        stats.bytes = stats.bytes.saturating_add(bytes);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn subtract_report_bytes(
        &self,
        bytes: usize,
    ) {
        let mut stats = self.report_stats.borrow_mut();
        stats.bytes = stats.bytes.saturating_sub(bytes);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn mark_seekable_dirty(
        &self,
    ) {
        self.report_stats.borrow_mut().seekable_dirty = true;
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn cached_seekable_summary(
        &self,
        generation: u64,
    ) -> Option<SeekableTimelineSummary> {
        let stats = self.report_stats.borrow();
        (!stats.seekable_dirty && stats.seekable_generation == Some(generation))
            .then(|| stats.seekable_summary.clone())
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn store_seekable_summary(
        &self,
        generation: u64,
        summary: SeekableTimelineSummary,
    ) {
        let mut stats = self.report_stats.borrow_mut();
        stats.seekable_summary = summary;
        stats.seekable_generation = Some(generation);
        stats.seekable_dirty = false;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct RangeReportStats {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) bytes: usize,
    seekable_summary: SeekableTimelineSummary,
    seekable_dirty: bool,
    seekable_generation: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct StreamRangeBoundary {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) is_bof: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) is_eof: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) seek_start_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) seek_end_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) last_pruned_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) pruned_packet_count: u64,
}

impl StreamRangeBoundary {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn new(
        is_bof: bool,
        is_eof: bool,
    ) -> Self {
        Self {
            is_bof,
            is_eof,
            seek_start_nsecs: None,
            seek_end_nsecs: None,
            last_pruned_nsecs: None,
            pruned_packet_count: 0,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct SeekableTimelineSummary {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) ranges: Vec<(u64, u64)>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) is_bof: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) is_eof: bool,
}

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxPacketRangeView<'a> {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) stream_queues:
        &'a BTreeMap<c_int, VecDeque<PacketId>>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) subtitle_stream_index:
        Option<c_int>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) is_bof: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) is_eof: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct ArchivedStreamPruneCandidate
{
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) stream_index: c_int,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) prune_always: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) seek_start_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) prune_count: usize,
}

#[derive(Clone, Debug)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxCachedSeekHit {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) range_id: RangeId,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) reader_heads:
        BTreeMap<c_int, PacketId>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) buffered_until_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) target_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) anchor_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) anchor_packet_id: PacketId,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) anchor_kind:
        VideoRecoveryPointKind,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) preroll_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) video_reader_head: PacketId,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) anchor_is_recovery_point:
        bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) anchor_is_safe_seek_point:
        bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) requires_precise_trim: bool,
}
