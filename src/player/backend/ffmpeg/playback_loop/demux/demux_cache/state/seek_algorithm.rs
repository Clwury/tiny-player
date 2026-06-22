use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    os::raw::c_int,
};

use super::{
    CachedDemuxPacket, DemuxCachedSeekHit, DemuxPacketCacheState, DemuxPacketRangeView, PacketId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CachedSeekTimelineBounds {
    first_cached_nsecs: u64,
    buffered_until_nsecs: u64,
    is_bof: bool,
    is_eof: bool,
}

fn cached_seek_target_in_bounds(
    bounds: CachedSeekTimelineBounds,
    target_nsecs: u64,
) -> Option<u64> {
    if (target_nsecs < bounds.first_cached_nsecs && !bounds.is_bof)
        || (target_nsecs > bounds.buffered_until_nsecs && !bounds.is_eof)
    {
        return None;
    }
    Some(target_nsecs.clamp(bounds.first_cached_nsecs, bounds.buffered_until_nsecs))
}

impl DemuxPacketCacheState {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn seek_cached_in_packet_range(
        packets: &HashMap<u64, CachedDemuxPacket>,
        timeline_anchor_stream_index: c_int,
        cached_seek_preroll_nsecs: u64,
        range: DemuxPacketRangeView<'_>,
        target_nsecs: u64,
    ) -> Option<DemuxCachedSeekHit> {
        let (first_cached_nsecs, buffered_until_nsecs) =
            Self::cached_timeline_range_in_packet_range(
                packets,
                timeline_anchor_stream_index,
                range.stream_queues,
            )?;
        let seek_target_nsecs = cached_seek_target_in_bounds(
            CachedSeekTimelineBounds {
                first_cached_nsecs,
                buffered_until_nsecs,
                is_bof: range.is_bof,
                is_eof: range.is_eof,
            },
            target_nsecs,
        )?;

        let mut covering_anchor_index = None;
        let mut keyframe_anchor_index = None;
        let mut preroll_keyframe_anchor_index = None;
        for packet_id in Self::timeline_anchor_packet_ids_in_packet_range(
            packets,
            timeline_anchor_stream_index,
            range.stream_queues,
        ) {
            let packet = packets.get(&packet_id)?;
            let start_nsecs = packet.start_nsecs?;
            let end_nsecs = packet.end_nsecs.unwrap_or(start_nsecs);
            let packet_index = range
                .global_order
                .iter()
                .position(|candidate| *candidate == packet_id);
            if packet.recovery_point && start_nsecs <= seek_target_nsecs {
                if cached_seek_preroll_nsecs > 0 {
                    preroll_keyframe_anchor_index = keyframe_anchor_index;
                }
                keyframe_anchor_index = packet_index;
            }
            if covering_anchor_index.is_none()
                && start_nsecs <= seek_target_nsecs
                && seek_target_nsecs <= end_nsecs
            {
                covering_anchor_index = packet_index;
            }
        }

        let _covering_anchor_index = covering_anchor_index?;
        let read_index = if cached_seek_preroll_nsecs > 0 {
            let read_index = preroll_keyframe_anchor_index?;
            let required_preroll_start =
                seek_target_nsecs.saturating_sub(cached_seek_preroll_nsecs);
            let packet_id = *range.global_order.get(read_index)?;
            let read_start_nsecs = packets.get(&packet_id)?.start_nsecs?;
            if read_start_nsecs > required_preroll_start {
                return None;
            }
            read_index
        } else {
            keyframe_anchor_index?
        };
        let anchor_packet_id = *range.global_order.get(read_index)?;
        let anchor_seek_target_nsecs = packets
            .get(&anchor_packet_id)?
            .start_nsecs
            .unwrap_or(seek_target_nsecs);
        let mut reader_heads = BTreeMap::new();
        for (stream_index, queue) in range.stream_queues {
            let packet_id = if *stream_index == timeline_anchor_stream_index {
                Some(anchor_packet_id)
            } else {
                Self::find_stream_seek_target_in_packet_queue(
                    packets,
                    timeline_anchor_stream_index,
                    *stream_index,
                    queue,
                    anchor_seek_target_nsecs,
                )
            };
            if let Some(packet_id) = packet_id {
                reader_heads.insert(*stream_index, packet_id);
            }
        }
        Some(DemuxCachedSeekHit {
            reader_heads,
            buffered_until_nsecs,
        })
    }

    fn find_stream_seek_target_in_packet_queue(
        packets: &HashMap<u64, CachedDemuxPacket>,
        timeline_anchor_stream_index: c_int,
        stream_index: c_int,
        queue: &VecDeque<u64>,
        target_nsecs: u64,
    ) -> Option<PacketId> {
        let mut target = None;
        for packet_id in queue {
            let Some(packet) = packets.get(packet_id) else {
                continue;
            };
            if !Self::packet_is_stream_seek_boundary_for(
                timeline_anchor_stream_index,
                stream_index,
                packet,
            ) {
                continue;
            }
            let Some(start_nsecs) = packet.start_nsecs else {
                continue;
            };
            if target.is_some() && start_nsecs > target_nsecs {
                break;
            }
            target = Some(*packet_id);
        }
        target
    }

    fn timeline_anchor_packet_ids_in_packet_range<'a>(
        packets: &'a HashMap<u64, CachedDemuxPacket>,
        timeline_anchor_stream_index: c_int,
        stream_queues: &'a BTreeMap<c_int, VecDeque<u64>>,
    ) -> impl Iterator<Item = u64> + 'a {
        stream_queues
            .get(&timeline_anchor_stream_index)
            .into_iter()
            .flat_map(|queue| queue.iter().copied())
            .filter(|packet_id| {
                packets
                    .get(packet_id)
                    .is_some_and(|packet| packet.timeline_anchor && packet.start_nsecs.is_some())
            })
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn cached_timeline_range_in_packet_range(
        packets: &HashMap<u64, CachedDemuxPacket>,
        timeline_anchor_stream_index: c_int,
        stream_queues: &BTreeMap<c_int, VecDeque<u64>>,
    ) -> Option<(u64, u64)> {
        let mut first_cached_nsecs = None;
        let mut buffered_until_nsecs = None;
        for packet_id in Self::timeline_anchor_packet_ids_in_packet_range(
            packets,
            timeline_anchor_stream_index,
            stream_queues,
        ) {
            let packet = packets.get(&packet_id)?;
            let start_nsecs = packet.start_nsecs?;
            let end_nsecs = packet.end_nsecs.unwrap_or(start_nsecs);
            first_cached_nsecs = Some(first_cached_nsecs.unwrap_or(start_nsecs).min(start_nsecs));
            buffered_until_nsecs = Some(buffered_until_nsecs.unwrap_or(end_nsecs).max(end_nsecs));
        }
        first_cached_nsecs.zip(buffered_until_nsecs)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn seekable_timeline_ranges_in_packet_range(
        packets: &HashMap<u64, CachedDemuxPacket>,
        timeline_anchor_stream_index: c_int,
        cached_seek_preroll_nsecs: u64,
        stream_queues: &BTreeMap<c_int, VecDeque<u64>>,
        close_open_segment: bool,
    ) -> Vec<(u64, u64)> {
        Self::seekable_timeline_range_in_packet_range(
            packets,
            timeline_anchor_stream_index,
            cached_seek_preroll_nsecs,
            stream_queues,
            close_open_segment,
        )
        .into_iter()
        .collect()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn seekable_timeline_range_in_packet_range(
        packets: &HashMap<u64, CachedDemuxPacket>,
        timeline_anchor_stream_index: c_int,
        cached_seek_preroll_nsecs: u64,
        stream_queues: &BTreeMap<c_int, VecDeque<u64>>,
        close_open_segment: bool,
    ) -> Option<(u64, u64)> {
        let mut seek_start_nsecs = None;
        let mut seek_end_nsecs = None;
        let mut current_block: Option<VideoSeekBlock> = None;
        let mut previous_recovery_start_nsecs = None;

        for packet_id in Self::timeline_anchor_packet_ids_in_packet_range(
            packets,
            timeline_anchor_stream_index,
            stream_queues,
        ) {
            let Some(packet) = packets.get(&packet_id) else {
                continue;
            };
            let Some(start_nsecs) = packet.start_nsecs else {
                continue;
            };
            let end_nsecs = packet.end_nsecs.unwrap_or(start_nsecs);

            if packet.recovery_point {
                if let Some(block) = current_block.take() {
                    Self::close_video_seek_block(
                        block,
                        cached_seek_preroll_nsecs,
                        &mut seek_start_nsecs,
                        &mut seek_end_nsecs,
                    );
                }

                current_block = Some(VideoSeekBlock {
                    min_nsecs: start_nsecs,
                    max_nsecs: end_nsecs,
                    recovery_start_nsecs: start_nsecs,
                    previous_recovery_start_nsecs,
                });
                previous_recovery_start_nsecs = Some(start_nsecs);
            } else if let Some(block) = current_block.as_mut() {
                block.min_nsecs = block.min_nsecs.min(start_nsecs);
                block.max_nsecs = block.max_nsecs.max(end_nsecs);
            }
        }

        if close_open_segment && let Some(block) = current_block {
            Self::close_video_seek_block(
                block,
                cached_seek_preroll_nsecs,
                &mut seek_start_nsecs,
                &mut seek_end_nsecs,
            );
        }
        seek_start_nsecs
            .zip(seek_end_nsecs)
            .filter(|(start_nsecs, end_nsecs)| end_nsecs > start_nsecs)
    }

    fn close_video_seek_block(
        block: VideoSeekBlock,
        cached_seek_preroll_nsecs: u64,
        seek_start_out: &mut Option<u64>,
        seek_end_out: &mut Option<u64>,
    ) {
        let Some(block_seek_start_nsecs) =
            Self::video_seek_block_start_nsecs(block, cached_seek_preroll_nsecs)
        else {
            return;
        };
        *seek_start_out = Some(seek_start_out.unwrap_or(block_seek_start_nsecs));
        *seek_end_out = Some(seek_end_out.unwrap_or(block.max_nsecs).max(block.max_nsecs));
    }

    fn video_seek_block_start_nsecs(
        block: VideoSeekBlock,
        cached_seek_preroll_nsecs: u64,
    ) -> Option<u64> {
        if cached_seek_preroll_nsecs == 0 {
            return Some(block.min_nsecs);
        }
        block.previous_recovery_start_nsecs.map(|previous_start| {
            if previous_start == 0 {
                block.min_nsecs
            } else {
                block
                    .recovery_start_nsecs
                    .max(previous_start.saturating_add(cached_seek_preroll_nsecs))
            }
        })
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn stream_seek_range_in_packet_queue(
        packets: &HashMap<u64, CachedDemuxPacket>,
        queue: &VecDeque<u64>,
    ) -> Option<(u64, u64)> {
        let mut seek_start_nsecs = None;
        let mut seek_end_nsecs = None;
        for packet_id in queue {
            let Some(packet) = packets.get(packet_id) else {
                continue;
            };
            let Some(start_nsecs) = packet.start_nsecs else {
                continue;
            };
            let end_nsecs = packet.end_nsecs.unwrap_or(start_nsecs);
            if end_nsecs <= start_nsecs {
                continue;
            }
            seek_start_nsecs = Some(seek_start_nsecs.unwrap_or(start_nsecs).min(start_nsecs));
            seek_end_nsecs = Some(seek_end_nsecs.unwrap_or(end_nsecs).max(end_nsecs));
        }
        seek_start_nsecs.zip(seek_end_nsecs)
    }
}

#[derive(Clone, Copy)]
struct VideoSeekBlock {
    min_nsecs: u64,
    max_nsecs: u64,
    recovery_start_nsecs: u64,
    previous_recovery_start_nsecs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::{CachedSeekTimelineBounds, cached_seek_target_in_bounds};

    fn bounds(is_bof: bool, is_eof: bool) -> CachedSeekTimelineBounds {
        CachedSeekTimelineBounds {
            first_cached_nsecs: 1_000,
            buffered_until_nsecs: 2_000,
            is_bof,
            is_eof,
        }
    }

    #[test]
    fn cached_seek_target_rejects_outside_non_edge_range() {
        assert_eq!(
            cached_seek_target_in_bounds(bounds(false, false), 999),
            None
        );
        assert_eq!(
            cached_seek_target_in_bounds(bounds(false, false), 2_001),
            None
        );
    }

    #[test]
    fn cached_seek_target_clamps_to_bof_or_eof_edge() {
        assert_eq!(
            cached_seek_target_in_bounds(bounds(true, false), 999),
            Some(1_000)
        );
        assert_eq!(
            cached_seek_target_in_bounds(bounds(false, true), 2_001),
            Some(2_000)
        );
    }

    #[test]
    fn cached_seek_target_accepts_inside_range() {
        assert_eq!(
            cached_seek_target_in_bounds(bounds(false, false), 1_500),
            Some(1_500)
        );
    }
}
