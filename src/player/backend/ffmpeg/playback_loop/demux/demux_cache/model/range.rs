use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    os::raw::c_int,
    time::Duration,
};

use super::types::{PacketId, RangeId};
use super::{PlaybackCacheTimeRange, VideoRecoveryPointKind};

const MAX_INTERNAL_PACKET_TIMESTAMP_HOLE_DETAILS: usize = 16;

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

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn record_rounding_gap(
        &self,
    ) {
        let mut stats = self.report_stats.borrow_mut();
        stats.rounding_gaps_merged = stats.rounding_gaps_merged.saturating_add(1);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn record_internal_packet_timestamp_hole(
        &self,
        hole: InternalPacketTimestampHole,
    ) {
        let mut stats = self.report_stats.borrow_mut();
        stats.internal_packet_timestamp_hole_count =
            stats.internal_packet_timestamp_hole_count.saturating_add(1);
        if stats.internal_packet_timestamp_holes.len() < MAX_INTERNAL_PACKET_TIMESTAMP_HOLE_DETAILS
        {
            stats.internal_packet_timestamp_holes.push(hole);
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn seekable_diagnostic_stats(
        &self,
    ) -> SeekableRangeValidationStats {
        let stats = self.report_stats.borrow();
        SeekableRangeValidationStats {
            rounding_gaps_merged: stats.rounding_gaps_merged,
            internal_packet_timestamp_holes: stats.internal_packet_timestamp_hole_count,
            ..SeekableRangeValidationStats::default()
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn internal_packet_timestamp_hole_details(
        &self,
    ) -> Vec<InternalPacketTimestampHole> {
        self.report_stats
            .borrow()
            .internal_packet_timestamp_holes
            .clone()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct RangeReportStats {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) bytes: usize,
    seekable_summary: SeekableTimelineSummary,
    seekable_dirty: bool,
    seekable_generation: Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) rounding_gaps_merged: usize,
    internal_packet_timestamp_hole_count: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) internal_packet_timestamp_holes:
        Vec<InternalPacketTimestampHole>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct InternalPacketTimestampHole
{
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) stream_index: c_int,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) previous_packet_id: PacketId,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) packet_id: PacketId,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) previous_raw_pts:
        Option<i64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) previous_raw_dts:
        Option<i64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) raw_pts: Option<i64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) raw_dts: Option<i64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) previous_seek_timestamp_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) seek_timestamp_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) previous_mapped_end_nsecs:
        u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) mapped_start_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) gap_nsecs: u64,
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
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) validation:
        SeekableRangeValidationStats,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct PreparedSeekableRangeReport
{
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) generation: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) revision: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) ranges:
        Vec<PlaybackCacheTimeRange>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) bof_cached: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) eof_cached: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) validation:
        SeekableRangeValidationStats,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct SeekableRangeValidationStats
{
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) validation_packets: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) validation_probes: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) rounding_gaps_merged: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) internal_packet_timestamp_holes:
        usize,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) elapsed: Duration,
}

impl SeekableRangeValidationStats {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn merged(
        self,
        other: Self,
    ) -> Self {
        Self {
            validation_packets: self
                .validation_packets
                .saturating_add(other.validation_packets),
            validation_probes: self
                .validation_probes
                .saturating_add(other.validation_probes),
            rounding_gaps_merged: self
                .rounding_gaps_merged
                .saturating_add(other.rounding_gaps_merged),
            internal_packet_timestamp_holes: self
                .internal_packet_timestamp_holes
                .saturating_add(other.internal_packet_timestamp_holes),
            elapsed: self.elapsed.saturating_add(other.elapsed),
        }
    }
}

#[derive(Clone, Copy)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) enum CachedSeekMissReason {
    TargetOutsideRange,
    MissingPrerollAnchor,
    MissingStreamReaderHead,
    GenerationBlocked,
    AnchorTrimmed,
}

impl CachedSeekMissReason {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn as_str(
        self,
    ) -> &'static str {
        match self {
            Self::TargetOutsideRange => "target_outside_range",
            Self::MissingPrerollAnchor => "missing_preroll_anchor",
            Self::MissingStreamReaderHead => "missing_stream_reader_head",
            Self::GenerationBlocked => "generation_blocked",
            Self::AnchorTrimmed => "anchor_trimmed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct CachedSeekMiss {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) range_id: Option<RangeId>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) target_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) reason: CachedSeekMissReason,
}
