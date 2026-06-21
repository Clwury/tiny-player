use std::{
    collections::{BTreeMap, VecDeque},
    os::raw::c_int,
};

use super::types::{PacketId, RangeId};

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxCachedRange {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) id: RangeId,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) global_order:
        VecDeque<PacketId>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) stream_queues:
        BTreeMap<c_int, VecDeque<PacketId>>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) sparse_stream_pruned_until_nsecs:
        BTreeMap<c_int, u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) stream_boundaries:
        BTreeMap<c_int, StreamRangeBoundary>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) is_bof: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) is_eof: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) last_used_generation: u64,
}

impl DemuxCachedRange {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn stream_boundary(
        &self,
        stream_index: c_int,
    ) -> StreamRangeBoundary {
        self.stream_boundaries
            .get(&stream_index)
            .copied()
            .unwrap_or(StreamRangeBoundary {
                is_bof: self.is_bof,
                is_eof: self.is_eof,
            })
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn ensure_stream_boundary(
        &mut self,
        stream_index: c_int,
    ) -> &mut StreamRangeBoundary {
        let default_boundary = StreamRangeBoundary {
            is_bof: self.is_bof,
            is_eof: self.is_eof,
        };
        self.stream_boundaries
            .entry(stream_index)
            .or_insert(default_boundary)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct StreamRangeBoundary {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) is_bof: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) is_eof: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct SeekableTimelineSummary {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) ranges: Vec<(u64, u64)>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) is_bof: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) is_eof: bool,
}

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxPacketRangeView<'a> {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) global_order:
        &'a VecDeque<PacketId>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) stream_queues:
        &'a BTreeMap<c_int, VecDeque<PacketId>>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) is_bof: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) is_eof: bool,
}

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct ArchivedStreamPruneCandidate
{
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) stream_index: c_int,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) prune_always: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) seek_start_nsecs:
        Option<u64>,
}

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxCachedSeekHit {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) reader_heads:
        BTreeMap<c_int, PacketId>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) buffered_until_nsecs: u64,
}
