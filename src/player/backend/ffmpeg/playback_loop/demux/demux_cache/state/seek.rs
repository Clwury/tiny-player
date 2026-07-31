use std::{collections::BTreeMap, os::raw::c_int};

use super::seek_algorithm::CachedSeekPacketRangeContext;
use super::{
    CachedSeekMiss, CachedSeekMissReason, DemuxCachedRange, DemuxCachedSeekHit,
    DemuxCachedSeekInfo, DemuxCachedSeekPlan, DemuxPacketCacheState, DemuxPacketRangeView,
    DemuxSeekRequest, PacketId, PlaybackSeekMode, PlaybackSessionId, RangeId, StreamCacheKind,
    nsecs_to_seconds,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CachedSeekRangeLocation {
    Current,
    Detached,
    Archived,
}

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

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn seek_cached_with_generation(
        &mut self,
        target_nsecs: u64,
        mode: PlaybackSeekMode,
        session_id: PlaybackSessionId,
        seek_generation: u64,
    ) -> Option<f64> {
        self.seek_cached_with_generation_hit(target_nsecs, mode, session_id, seek_generation)
            .map(|hit| nsecs_to_seconds(hit.buffered_until_nsecs))
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn seek_cached_with_generation_hit(
        &mut self,
        target_nsecs: u64,
        mode: PlaybackSeekMode,
        session_id: PlaybackSessionId,
        seek_generation: u64,
    ) -> Option<DemuxCachedSeekHit> {
        self.seek_cached_with_generation_attempt(target_nsecs, mode, session_id, seek_generation)
            .ok()
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn seek_cached_with_generation_attempt(
        &mut self,
        target_nsecs: u64,
        mode: PlaybackSeekMode,
        session_id: PlaybackSessionId,
        seek_generation: u64,
    ) -> Result<DemuxCachedSeekHit, CachedSeekMiss> {
        let plan = match self.resolve_cached_seek_plan_attempt(target_nsecs, mode) {
            Ok(plan) => plan,
            Err(miss) => {
                self.record_cached_seek_rejection(miss);
                return Err(miss);
            }
        };
        self.commit_cached_seek_plan(plan, session_id, seek_generation)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn resolve_cached_seek_plan_attempt(
        &self,
        target_nsecs: u64,
        mode: PlaybackSeekMode,
    ) -> Result<DemuxCachedSeekPlan, CachedSeekMiss> {
        let detached_append_range_id = self.detached_append_range_id();
        let mut ordered_ranges = vec![(self.read_range_id, CachedSeekRangeLocation::Current)];
        if let Some(range_id) = detached_append_range_id {
            ordered_ranges.push((range_id, CachedSeekRangeLocation::Detached));
        }
        ordered_ranges.extend(
            self.ranges
                .keys()
                .copied()
                .filter(|range_id| *range_id != self.read_range_id)
                .filter(|range_id| Some(*range_id) != detached_append_range_id)
                .map(|range_id| (range_id, CachedSeekRangeLocation::Archived)),
        );

        let mut first_rejection = None;
        for (range_id, location) in ordered_ranges {
            if self.failed_cached_seek_ranges.contains_key(&range_id)
                || self.rejected_cached_seek_ranges.contains_key(&range_id)
            {
                continue;
            }
            let Some(range) = self.ranges.get(&range_id) else {
                continue;
            };
            if self.range_cached_seek_target(range, target_nsecs).is_none() {
                continue;
            }
            match self.seek_cached_in_range_diagnostic(range, target_nsecs, mode) {
                Ok(hit) => {
                    let _ = location;
                    return Ok(DemuxCachedSeekPlan {
                        hit,
                        seekability_revision: self.seekability_revision(),
                        cache_generation: self.generation,
                        read_range_id: self.read_range_id,
                        append_range_id: self.append_range_id,
                    });
                }
                Err(miss) => {
                    first_rejection.get_or_insert(miss);
                }
            }
        }

        Err(first_rejection.unwrap_or(CachedSeekMiss {
            range_id: None,
            target_nsecs,
            reason: CachedSeekMissReason::TargetOutsideRange,
        }))
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn record_cached_seek_rejection(
        &mut self,
        miss: CachedSeekMiss,
    ) {
        let Some(range_id) = miss.range_id else {
            return;
        };
        self.rejected_cached_seek_ranges.insert(range_id, miss);
        if let Some(range) = self.ranges.get(&range_id) {
            range.mark_seekable_dirty();
        }
        self.bump_seekability_revision();
        tracing::warn!(
            session_id = ?self.session_id,
            range_id,
            target_nsecs = miss.target_nsecs,
            rejection_reason = miss.reason.as_str(),
            "invalidated advertised FFmpeg cached seek range after exact seek rejection"
        );
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn commit_cached_seek_plan(
        &mut self,
        plan: DemuxCachedSeekPlan,
        session_id: PlaybackSessionId,
        seek_generation: u64,
    ) -> Result<DemuxCachedSeekHit, CachedSeekMiss> {
        if !self.cached_seek_plan_is_current(&plan) {
            return Err(CachedSeekMiss {
                range_id: Some(plan.hit.range_id),
                target_nsecs: plan.hit.target_nsecs,
                reason: CachedSeekMissReason::GenerationBlocked,
            });
        }
        let location = if plan.hit.range_id == self.read_range_id {
            CachedSeekRangeLocation::Current
        } else if Some(plan.hit.range_id) == self.detached_append_range_id() {
            CachedSeekRangeLocation::Detached
        } else {
            CachedSeekRangeLocation::Archived
        };
        Ok(self.commit_cached_seek_hit(plan.hit, location, session_id, seek_generation))
    }

    fn cached_seek_plan_is_current(&self, plan: &DemuxCachedSeekPlan) -> bool {
        if plan.seekability_revision != self.seekability_revision()
            || plan.cache_generation != self.generation
            || plan.read_range_id != self.read_range_id
            || plan.append_range_id != self.append_range_id
            || self
                .failed_cached_seek_ranges
                .contains_key(&plan.hit.range_id)
            || self
                .rejected_cached_seek_ranges
                .contains_key(&plan.hit.range_id)
        {
            return false;
        }
        let Some(range) = self.ranges.get(&plan.hit.range_id) else {
            return false;
        };
        if self
            .range_cached_seek_target(range, plan.hit.target_nsecs)
            .is_none()
        {
            return false;
        }
        plan.hit
            .reader_heads
            .iter()
            .all(|(stream_index, packet_id)| {
                self.packets.contains_key(packet_id)
                    && range
                        .stream_queues
                        .get(stream_index)
                        .is_some_and(|queue| queue.contains(packet_id))
            })
    }

    fn commit_cached_seek_hit(
        &mut self,
        hit: DemuxCachedSeekHit,
        location: CachedSeekRangeLocation,
        session_id: PlaybackSessionId,
        seek_generation: u64,
    ) -> DemuxCachedSeekHit {
        let result = hit.clone();
        let buffered_until_nsecs = hit.buffered_until_nsecs;
        if location != CachedSeekRangeLocation::Current {
            self.preserve_current_range();
        }
        self.generation = self.generation.saturating_add(1);
        self.bump_seekability_revision();
        if location == CachedSeekRangeLocation::Current {
            self.set_reader_heads_for_current_generation(hit.reader_heads);
            self.refresh_reader_tracking();
        } else {
            self.activate_range_for_read_with_heads(hit.range_id, hit.reader_heads);
        }
        self.reader_nsecs = hit.anchor_nsecs;
        self.exact_seek_target_nsecs = hit.target_nsecs;
        self.session_id = session_id;
        self.seek_request = None;
        self.seeking = false;
        self.resume_append_skip_until_nsecs = None;
        self.low_level_append_guard_target_nsecs = None;
        if location == CachedSeekRangeLocation::Archived && !self.read_range_eof() {
            self.queue_resume_seek_after_cached_range(buffered_until_nsecs, seek_generation);
        }
        // A seek commit is latency-sensitive. Do at most one bounded trim
        // step here; normal read/append maintenance finishes any remaining
        // backbuffer work without delaying the first target frame.
        let _ = self.trim_to_limit_for_read_with_outcome();
        self.cached_seeks = self.cached_seeks.saturating_add(1);
        self.refresh_readahead_hysteresis();
        result
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn exclude_failed_cached_seek_range(
        &mut self,
        info: DemuxCachedSeekInfo,
    ) -> bool {
        let changed = self
            .failed_cached_seek_ranges
            .insert(info.range_id, info)
            .is_none()
            && self.ranges.contains_key(&info.range_id);
        if changed {
            if let Some(range) = self.ranges.get(&info.range_id) {
                range.mark_seekable_dirty();
            }
            self.bump_seekability_revision();
        }
        tracing::warn!(
            session_id = ?self.session_id,
            range_id = info.range_id,
            anchor_packet_id = info.anchor_packet_id,
            anchor_kind = info.anchor_kind.as_str(),
            anchor_nsecs = info.anchor_nsecs,
            target_nsecs = info.target_nsecs,
            preroll_nsecs = info.preroll_nsecs,
            previous_range_count = self
                .last_emitted_seekable_ranges
                .as_ref()
                .map(Vec::len)
                .unwrap_or_default(),
            seekability_revision = self.seekability_revision(),
            changed,
            "temporarily excluded failed cached seek recovery anchor for playback session"
        );
        changed
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn failed_cached_seek_range(
        &self,
        range_id: RangeId,
    ) -> Option<DemuxCachedSeekInfo> {
        self.failed_cached_seek_ranges.get(&range_id).copied()
    }

    fn seek_cached_in_range_diagnostic(
        &self,
        range: &DemuxCachedRange,
        target_nsecs: u64,
        mode: PlaybackSeekMode,
    ) -> Result<DemuxCachedSeekHit, CachedSeekMiss> {
        let seek_target =
            self.range_cached_seek_target(range, target_nsecs)
                .ok_or(CachedSeekMiss {
                    range_id: None,
                    target_nsecs,
                    reason: CachedSeekMissReason::TargetOutsideRange,
                })?;
        let (_, stream_queues) = self.next_generation_range_view(range);
        let required_stream_indices = self
            .stream_kinds
            .iter()
            .filter_map(|(stream_index, kind)| {
                matches!(kind, StreamCacheKind::Video | StreamCacheKind::Audio)
                    .then_some(*stream_index)
            })
            .collect::<Vec<_>>();
        let cached_seek_preroll_nsecs = match mode {
            PlaybackSeekMode::Precise => self.cached_seek_preroll_nsecs,
            PlaybackSeekMode::Fast => 0,
        };
        let attempt = Self::seek_cached_in_packet_range_diagnostic(
            CachedSeekPacketRangeContext {
                packets: &self.packets,
                range_id: range.id,
                timeline_anchor_stream_index: self.timeline_anchor_stream_index,
                cached_seek_preroll_nsecs,
                recovery_point_stream_index: self.recovery_point_stream_index(),
                required_stream_indices: &required_stream_indices,
                stream_pts_index: &range.stream_pts_index,
                stream_recovery_point_index: &range.stream_recovery_point_index,
                buffered_until_nsecs: seek_target.seekable_until_nsecs,
                range: DemuxPacketRangeView {
                    stream_queues: &stream_queues,
                    subtitle_stream_index: self
                        .selected_streams
                        .subtitle_stream
                        .map(|stream| stream.index),
                },
            },
            seek_target.target_nsecs,
        );
        let mut hit = match attempt {
            Ok(hit) => hit,
            Err(reason) => {
                let reason = self.classify_cached_seek_miss(
                    range,
                    seek_target.target_nsecs,
                    cached_seek_preroll_nsecs,
                    &required_stream_indices,
                    reason,
                );
                return Err(CachedSeekMiss {
                    range_id: Some(range.id),
                    target_nsecs: seek_target.target_nsecs,
                    reason,
                });
            }
        };
        hit.buffered_until_nsecs = hit
            .buffered_until_nsecs
            .min(seek_target.seekable_until_nsecs);
        Ok(hit)
    }

    fn classify_cached_seek_miss(
        &self,
        range: &DemuxCachedRange,
        target_nsecs: u64,
        cached_seek_preroll_nsecs: u64,
        required_stream_indices: &[c_int],
        reason: CachedSeekMissReason,
    ) -> CachedSeekMissReason {
        let next_generation = self.generation.saturating_add(1);
        let has_next_generation_block = range.global_order.iter().any(|packet_id| {
            self.low_level_append_blocked_packet_generations
                .get(packet_id)
                .is_some_and(|generation| *generation <= next_generation)
        });
        if has_next_generation_block
            && Self::seek_cached_in_packet_range_diagnostic(
                CachedSeekPacketRangeContext {
                    packets: &self.packets,
                    range_id: range.id,
                    timeline_anchor_stream_index: self.timeline_anchor_stream_index,
                    cached_seek_preroll_nsecs,
                    recovery_point_stream_index: self.recovery_point_stream_index(),
                    required_stream_indices,
                    stream_pts_index: &range.stream_pts_index,
                    stream_recovery_point_index: &range.stream_recovery_point_index,
                    buffered_until_nsecs: self
                        .range_cached_seek_target(range, target_nsecs)
                        .map(|target| target.seekable_until_nsecs)
                        .unwrap_or(target_nsecs),
                    range: DemuxPacketRangeView {
                        stream_queues: &range.stream_queues,
                        subtitle_stream_index: self
                            .selected_streams
                            .subtitle_stream
                            .map(|stream| stream.index),
                    },
                },
                target_nsecs,
            )
            .is_ok()
        {
            return CachedSeekMissReason::GenerationBlocked;
        }
        if matches!(
            reason,
            CachedSeekMissReason::MissingPrerollAnchor | CachedSeekMissReason::AnchorTrimmed
        ) && range
            .stream_boundary(self.timeline_anchor_stream_index)
            .pruned_packet_count
            > 0
        {
            return CachedSeekMissReason::AnchorTrimmed;
        }
        reason
    }

    fn range_cached_seek_target(
        &self,
        range: &DemuxCachedRange,
        target_nsecs: u64,
    ) -> Option<RangeCachedSeekTarget> {
        let summary = self.range_seekable_timeline_summary(range);
        let first = summary.ranges.first().copied()?;
        let last = summary.ranges.last().copied()?;

        if target_nsecs < first.0 {
            return summary.is_bof.then_some(RangeCachedSeekTarget {
                target_nsecs: first.0,
                seekable_until_nsecs: first.1,
            });
        }
        if target_nsecs > last.1 {
            return summary.is_eof.then_some(RangeCachedSeekTarget {
                target_nsecs: last.1,
                seekable_until_nsecs: last.1,
            });
        }
        summary
            .ranges
            .into_iter()
            .find(|(start, end)| *start <= target_nsecs && target_nsecs <= *end)
            .map(|(_, end)| RangeCachedSeekTarget {
                target_nsecs,
                seekable_until_nsecs: end,
            })
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
        self.reader_head_generations = self
            .reader_heads
            .keys()
            .copied()
            .map(|stream_index| (stream_index, self.generation))
            .collect();
        self.refresh_reader_tracking();
    }

    fn queue_resume_seek_after_cached_range(
        &mut self,
        buffered_until_nsecs: u64,
        seek_generation: u64,
    ) {
        self.demux_input_generation = self.demux_input_generation.saturating_add(1);
        self.seek_request = Some(DemuxSeekRequest {
            position_seconds: nsecs_to_seconds(buffered_until_nsecs),
            session_id: self.session_id,
            seek_generation,
        });
        self.demux_position_detached = false;
        self.resume_append_skip_until_nsecs = Some(buffered_until_nsecs);
        self.low_level_append_guard_target_nsecs = Some(buffered_until_nsecs);
        self.start_detached_append_range();
        self.seeking = true;
        self.low_level_seeks = self.low_level_seeks.saturating_add(1);
        self.demux_ts_nsecs = None;
        self.set_range_eof(self.read_range_id, false);
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
        self.exact_seek_target_nsecs = target_nsecs;
        self.session_id = session_id;
        self.seek_request = Some(DemuxSeekRequest {
            position_seconds,
            session_id,
            seek_generation,
        });
        self.demux_position_detached = false;
        self.resume_append_skip_until_nsecs = None;
        self.low_level_append_guard_target_nsecs = Some(target_nsecs);
        self.demux_input_generation = self.demux_input_generation.saturating_add(1);
        self.generation = self.generation.saturating_add(1);
        self.bump_seekability_revision();
        self.start_new_current_range(target_nsecs == 0);
        self.seeking = true;
        self.low_level_seeks = self.low_level_seeks.saturating_add(1);
        self.demux_ts_nsecs = None;
        self.hysteresis_active = false;
        let _ = self.trim_to_limit_for_read_with_outcome();
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
        self.exact_seek_target_nsecs = self.reader_nsecs;
        self.seek_request = Some(DemuxSeekRequest {
            position_seconds,
            session_id,
            seek_generation,
        });
        self.demux_position_detached = false;
        self.resume_append_skip_until_nsecs = None;
        self.low_level_append_guard_target_nsecs = Some(self.reader_nsecs);
        self.demux_input_generation = self.demux_input_generation.saturating_add(1);
        self.generation = self.generation.saturating_add(1);
        self.bump_seekability_revision();
        self.start_new_current_range(false);
        self.seeking = true;
        self.low_level_seeks = self.low_level_seeks.saturating_add(1);
        self.demux_ts_nsecs = None;
        self.hysteresis_active = false;
        let _ = self.trim_to_limit_for_read_with_outcome();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn take_seek_request(
        &mut self,
    ) -> Option<DemuxSeekRequest> {
        self.seek_request.take()
    }
}

struct RangeCachedSeekTarget {
    target_nsecs: u64,
    seekable_until_nsecs: u64,
}
