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
        range_id: super::RangeId,
        timeline_anchor_stream_index: c_int,
        cached_seek_preroll_nsecs: u64,
        recovery_point_stream_index: Option<c_int>,
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

        let anchor_search_nsecs = seek_target_nsecs.saturating_sub(cached_seek_preroll_nsecs);
        let mut covering_anchor_found = false;
        let mut recovery_anchor_packet_id = None;
        let mut safe_anchor_packet_id = None;
        for packet_id in Self::timeline_anchor_packet_ids_in_packet_range(
            packets,
            timeline_anchor_stream_index,
            range.stream_queues,
        ) {
            let packet = packets.get(&packet_id)?;
            let start_nsecs = packet.start_nsecs?;
            let end_nsecs = packet.end_nsecs.unwrap_or(start_nsecs);
            if packet.recovery_point && start_nsecs <= anchor_search_nsecs {
                recovery_anchor_packet_id = Some(packet_id);
                if packet.safe_seek_point {
                    safe_anchor_packet_id = Some(packet_id);
                }
            }
            if !covering_anchor_found
                && start_nsecs <= seek_target_nsecs
                && seek_target_nsecs <= end_nsecs
            {
                covering_anchor_found = true;
            }
        }

        covering_anchor_found.then_some(())?;
        // IDR/BLA are closed-GOP safe points and remain preferred. CRA is a
        // cached-seek-only fallback when the closed seekable interval proves
        // that all required preroll packets are resident.
        let anchor_packet_id = safe_anchor_packet_id.or(recovery_anchor_packet_id)?;
        let anchor_packet = packets.get(&anchor_packet_id)?;
        let anchor_is_recovery_point = anchor_packet.recovery_point;
        if !anchor_is_recovery_point {
            return None;
        }
        let anchor_is_safe_seek_point = anchor_packet.safe_seek_point;
        let anchor_seek_target_nsecs = anchor_packet.start_nsecs.unwrap_or(seek_target_nsecs);
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
                    recovery_point_stream_index == Some(*stream_index),
                    range.subtitle_stream_index == Some(*stream_index),
                )
            };
            if let Some(packet_id) = packet_id {
                reader_heads.insert(*stream_index, packet_id);
            }
        }
        let video_reader_head = reader_heads
            .get(&timeline_anchor_stream_index)
            .copied()
            .filter(|packet_id| *packet_id == anchor_packet_id)?;
        Some(DemuxCachedSeekHit {
            range_id,
            reader_heads,
            buffered_until_nsecs,
            target_nsecs: seek_target_nsecs,
            anchor_nsecs: anchor_seek_target_nsecs,
            anchor_packet_id,
            anchor_kind: anchor_packet.recovery_kind,
            preroll_nsecs: cached_seek_preroll_nsecs,
            video_reader_head,
            anchor_is_recovery_point,
            anchor_is_safe_seek_point,
            requires_precise_trim: anchor_seek_target_nsecs < seek_target_nsecs,
        })
    }

    pub(super) fn packet_is_cached_seek_anchor(packet: &CachedDemuxPacket) -> bool {
        packet.recovery_point
    }

    fn find_stream_seek_target_in_packet_queue(
        packets: &HashMap<u64, CachedDemuxPacket>,
        timeline_anchor_stream_index: c_int,
        stream_index: c_int,
        queue: &VecDeque<u64>,
        target_nsecs: u64,
        require_recovery_point: bool,
        prefer_first_packet_at_timestamp: bool,
    ) -> Option<PacketId> {
        let mut target = None;
        let mut target_start_nsecs = None;
        for packet_id in queue {
            let Some(packet) = packets.get(packet_id) else {
                continue;
            };
            if !Self::packet_is_stream_seek_boundary_for(
                timeline_anchor_stream_index,
                stream_index,
                packet,
                require_recovery_point,
            ) {
                continue;
            }
            let Some(start_nsecs) = packet.start_nsecs else {
                continue;
            };
            if target.is_some() && start_nsecs > target_nsecs {
                break;
            }
            if prefer_first_packet_at_timestamp && target_start_nsecs == Some(start_nsecs) {
                continue;
            }
            target = Some(*packet_id);
            target_start_nsecs = Some(start_nsecs);
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

            if Self::packet_is_cached_seek_anchor(packet) {
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
                    recovery_packet_id: packet_id,
                    recovery_kind: packet.recovery_kind,
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

    pub(super) fn close_video_seek_block(
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
        let first_seekable_from_this_recovery = block
            .recovery_start_nsecs
            .saturating_add(cached_seek_preroll_nsecs);
        Some(match block.previous_recovery_start_nsecs {
            Some(previous_start) => block
                .recovery_start_nsecs
                .max(previous_start.saturating_add(cached_seek_preroll_nsecs)),
            None => first_seekable_from_this_recovery,
        })
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn stream_seek_range_in_packet_queue(
        packets: &HashMap<u64, CachedDemuxPacket>,
        queue: &VecDeque<u64>,
        require_recovery_point: bool,
        close_open_segment: bool,
    ) -> Option<(u64, u64)> {
        if require_recovery_point {
            // Match mpv's keyframe_latest handling: a TrueHD/MLP major-sync opens
            // a block, and only the next major-sync (or EOF) makes that block seekable.
            // compute_keyframe_times() uses packet PTS/DTS only, not packet duration.
            let mut seek_start_nsecs = None;
            let mut seek_end_nsecs = None;
            let mut current_block = None;
            for packet_id in queue {
                let Some(packet) = packets.get(packet_id) else {
                    continue;
                };
                let Some(start_nsecs) = packet.start_nsecs else {
                    continue;
                };
                if packet.recovery_point {
                    if let Some(block) = current_block.take() {
                        Self::close_stream_seek_block(
                            block,
                            &mut seek_start_nsecs,
                            &mut seek_end_nsecs,
                        );
                    }
                    current_block = Some(StreamSeekBlock {
                        min_nsecs: start_nsecs,
                        max_nsecs: start_nsecs,
                    });
                } else if let Some(block) = current_block.as_mut() {
                    block.min_nsecs = block.min_nsecs.min(start_nsecs);
                    block.max_nsecs = block.max_nsecs.max(start_nsecs);
                }
            }
            if close_open_segment && let Some(block) = current_block {
                Self::close_stream_seek_block(block, &mut seek_start_nsecs, &mut seek_end_nsecs);
            }
            return seek_start_nsecs
                .zip(seek_end_nsecs)
                .filter(|(start_nsecs, end_nsecs)| end_nsecs > start_nsecs);
        }

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
            seek_start_nsecs = Some(seek_start_nsecs.unwrap_or(start_nsecs).min(start_nsecs));
            seek_end_nsecs = Some(seek_end_nsecs.unwrap_or(end_nsecs).max(end_nsecs));
        }
        seek_start_nsecs
            .zip(seek_end_nsecs)
            .filter(|(start_nsecs, end_nsecs)| end_nsecs > start_nsecs)
    }

    pub(super) fn close_stream_seek_block(
        block: StreamSeekBlock,
        seek_start_out: &mut Option<u64>,
        seek_end_out: &mut Option<u64>,
    ) {
        *seek_start_out = Some(seek_start_out.unwrap_or(block.min_nsecs));
        *seek_end_out = Some(seek_end_out.unwrap_or(block.max_nsecs).max(block.max_nsecs));
    }
}

#[derive(Clone, Copy)]
pub(super) struct VideoSeekBlock {
    pub(super) min_nsecs: u64,
    pub(super) max_nsecs: u64,
    pub(super) recovery_start_nsecs: u64,
    pub(super) previous_recovery_start_nsecs: Option<u64>,
    pub(super) recovery_packet_id: PacketId,
    pub(super) recovery_kind: super::VideoRecoveryPointKind,
}

#[derive(Clone, Copy)]
pub(super) struct StreamSeekBlock {
    pub(super) min_nsecs: u64,
    pub(super) max_nsecs: u64,
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
