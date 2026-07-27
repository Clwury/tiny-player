use std::{sync::atomic::Ordering, time::Instant};

use super::{
    BackendEvent, BackendEventKind, CachedSeekMiss, CachedSeekMissReason,
    DEMUX_PACKET_CACHE_STALL_LOG_AFTER, DEMUX_PACKET_CACHE_STALL_LOG_INTERVAL,
    DEMUX_PACKET_CACHE_WAIT_INTERVAL, DemuxCachedSeekInfo, DemuxCachedSeekPlan, DemuxPacketCache,
    DemuxSeekResult, PlaybackCacheConfig, PlaybackSeekMode, PlaybackSessionId, nsecs_to_seconds,
    seconds_to_nsecs,
};

fn require_safe_cached_seek_anchor(
    resolved: Result<DemuxCachedSeekPlan, CachedSeekMiss>,
    safe_anchor_only: bool,
) -> Result<DemuxCachedSeekPlan, CachedSeekMiss> {
    let plan = resolved?;
    if safe_anchor_only && !plan.hit.anchor_is_safe_seek_point {
        return Err(CachedSeekMiss {
            range_id: Some(plan.hit.range_id),
            target_nsecs: plan.hit.target_nsecs,
            reason: CachedSeekMissReason::SafeAnchorRequired,
        });
    }
    Ok(plan)
}

impl DemuxPacketCache {
    pub(in crate::player::backend::ffmpeg::playback_loop) fn set_output_backpressure_prefetch_paused(
        &self,
        paused: bool,
    ) -> bool {
        let changed = self
            .shared
            .output_backpressure_prefetch_paused
            .swap(paused, Ordering::AcqRel)
            != paused;
        if changed {
            self.shared.notify_ready();
            tracing::debug!(
                paused,
                reason = "output_gate_and_decoder_queue_full",
                "updated FFmpeg demux/HTTP prefetch pause for output backpressure"
            );
        }
        changed
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn set_playback_recovery_demand(
        &self,
        critical: bool,
        video_required: bool,
        audio_required: bool,
    ) {
        self.shared
            .set_playback_recovery_demand(critical, video_required, audio_required);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn seek(
        &self,
        position_seconds: f64,
        mode: PlaybackSeekMode,
        session_id: PlaybackSessionId,
        seek_generation: u64,
    ) -> DemuxSeekResult {
        self.seek_with_policy(
            position_seconds,
            mode,
            session_id,
            seek_generation,
            true,
            false,
        )
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn seek_cached_only(
        &self,
        position_seconds: f64,
        mode: PlaybackSeekMode,
        session_id: PlaybackSessionId,
        seek_generation: u64,
    ) -> DemuxSeekResult {
        self.seek_with_policy(
            position_seconds,
            mode,
            session_id,
            seek_generation,
            false,
            false,
        )
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn seek_cached_safe_only(
        &self,
        position_seconds: f64,
        mode: PlaybackSeekMode,
        session_id: PlaybackSessionId,
        seek_generation: u64,
    ) -> DemuxSeekResult {
        self.seek_with_policy(
            position_seconds,
            mode,
            session_id,
            seek_generation,
            false,
            true,
        )
    }

    fn seek_with_policy(
        &self,
        position_seconds: f64,
        mode: PlaybackSeekMode,
        session_id: PlaybackSessionId,
        seek_generation: u64,
        low_level_on_miss: bool,
        safe_anchor_only: bool,
    ) -> DemuxSeekResult {
        let seek_started_at = Instant::now();
        let position_seconds = position_seconds.max(0.0);
        let target_nsecs = seconds_to_nsecs(position_seconds);
        let resolve_lock_started_at = Instant::now();
        let guard = self
            .shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let mut cache_lock_wait = resolve_lock_started_at.elapsed();
        let resolved_seekability_revision = guard.seekability_revision();
        let resolved_cache_generation = guard.generation;
        let lookup_started_at = Instant::now();
        let resolved = require_safe_cached_seek_anchor(
            guard.resolve_cached_seek_plan_attempt(target_nsecs, mode),
            safe_anchor_only,
        );
        let mut lookup = lookup_started_at.elapsed();
        drop(guard);

        // A later target may arrive while a deliberately slow cache lookup is
        // in progress. Never commit or flush output for the stale target.
        if self.shared.control.has_pending_seek()
            && self.shared.control.seek_generation() != seek_generation
        {
            tracing::debug!(
                ?session_id,
                position_seconds,
                ?mode,
                seek_generation,
                latest_seek_generation = self.shared.control.seek_generation(),
                cache_lock_wait_ms = cache_lock_wait.as_secs_f64() * 1_000.0,
                lookup_ms = lookup.as_secs_f64() * 1_000.0,
                "discarded superseded cached seek plan before commit"
            );
            return DemuxSeekResult::Superseded;
        }

        let commit_lock_started_at = Instant::now();
        let (result, should_enter_initial_cache_pause, cache_snapshot, buffered_changed) = {
            let mut guard = self
                .shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            cache_lock_wait += commit_lock_started_at.elapsed();
            guard.error = None;
            if self.shared.control.has_pending_seek()
                && self.shared.control.seek_generation() != seek_generation
            {
                tracing::debug!(
                    ?session_id,
                    position_seconds,
                    ?mode,
                    seek_generation,
                    latest_seek_generation = self.shared.control.seek_generation(),
                    cache_lock_wait_ms = cache_lock_wait.as_secs_f64() * 1_000.0,
                    lookup_ms = lookup.as_secs_f64() * 1_000.0,
                    "discarded superseded cached seek plan at commit barrier"
                );
                return DemuxSeekResult::Superseded;
            }

            let commit_started_at = Instant::now();
            let state_changed = guard.seekability_revision() != resolved_seekability_revision
                || guard.generation != resolved_cache_generation;
            let resolved = if state_changed {
                let retry_lookup_started_at = Instant::now();
                let resolved = require_safe_cached_seek_anchor(
                    guard.resolve_cached_seek_plan_attempt(target_nsecs, mode),
                    safe_anchor_only,
                );
                lookup += retry_lookup_started_at.elapsed();
                resolved
            } else {
                resolved
            };
            let committed = match resolved {
                Ok(plan) => {
                    match guard.commit_cached_seek_plan(plan, session_id, seek_generation) {
                        Ok(hit) => Ok(hit),
                        Err(_) => {
                            let retry_lookup_started_at = Instant::now();
                            let retry = require_safe_cached_seek_anchor(
                                guard.resolve_cached_seek_plan_attempt(target_nsecs, mode),
                                safe_anchor_only,
                            );
                            lookup += retry_lookup_started_at.elapsed();
                            retry.and_then(|plan| {
                                guard.commit_cached_seek_plan(plan, session_id, seek_generation)
                            })
                        }
                    }
                }
                Err(miss) => Err(miss),
            };
            let commit = commit_started_at.elapsed();
            match committed {
                Ok(hit) => {
                    let cached_seek_info = DemuxCachedSeekInfo {
                        range_id: hit.range_id,
                        target_nsecs: hit.target_nsecs,
                        anchor_nsecs: hit.anchor_nsecs,
                        preroll_nsecs: hit.preroll_nsecs,
                        anchor_packet_id: hit.anchor_packet_id,
                        anchor_kind: hit.anchor_kind,
                        anchor_is_safe_seek_point: hit.anchor_is_safe_seek_point,
                        requires_precise_trim: hit.requires_precise_trim,
                    };
                    let buffered_until = nsecs_to_seconds(hit.buffered_until_nsecs);
                    let audio_reader_head = guard
                        .selected_streams
                        .audio_stream
                        .and_then(|stream| hit.reader_heads.get(&stream.index).copied());
                    let subtitle_reader_head = guard
                        .selected_streams
                        .subtitle_stream
                        .and_then(|stream| hit.reader_heads.get(&stream.index).copied());
                    let subtitle_reader_head_start_nsecs = guard
                        .selected_streams
                        .subtitle_stream
                        .and_then(|stream| guard.stream_reader_head_timeline(stream.index))
                        .and_then(|(_, start_nsecs, _)| start_nsecs);
                    tracing::debug!(
                        ?session_id,
                        position_seconds,
                        ?mode,
                        target_nsecs,
                        cached_seek_target_nsecs = hit.target_nsecs,
                        anchor_nsecs = hit.anchor_nsecs,
                        anchor_packet_id = hit.anchor_packet_id,
                        anchor_kind = hit.anchor_kind.as_str(),
                        video_reader_head = hit.video_reader_head,
                        ?audio_reader_head,
                        ?subtitle_reader_head,
                        ?subtitle_reader_head_start_nsecs,
                        anchor_is_recovery_point = hit.anchor_is_recovery_point,
                        anchor_is_safe_seek_point = hit.anchor_is_safe_seek_point,
                        cached_seek_preroll_nsecs = hit.preroll_nsecs,
                        requires_precise_trim = hit.requires_precise_trim,
                        seek_generation,
                        buffered_until,
                        read_index = guard.read_index,
                        generation = guard.generation,
                        cache_lock_wait_ms = cache_lock_wait.as_secs_f64() * 1_000.0,
                        lookup_ms = lookup.as_secs_f64() * 1_000.0,
                        commit_ms = commit.as_secs_f64() * 1_000.0,
                        "FFmpeg demux packet cache seek hit"
                    );
                    let cache_snapshot =
                        guard.cache_report_snapshot(self.shared.control.is_cache_paused());
                    let buffered_changed =
                        guard.take_buffered_changed_for_cache_end(cache_snapshot.cache_end());
                    guard.record_cache_state_emit(Instant::now());
                    guard.record_emitted_seekable_ranges(cache_snapshot.seekable_ranges().clone());
                    self.shared.refresh_monitor_snapshot(&guard);
                    (
                        DemuxSeekResult::Cached(cached_seek_info),
                        false,
                        cache_snapshot,
                        buffered_changed,
                    )
                }
                Err(miss) => {
                    let policy_rejection = miss.reason == CachedSeekMissReason::SafeAnchorRequired;
                    if !policy_rejection {
                        guard.record_cached_seek_rejection(miss);
                    }
                    if let Some(range_id) = miss.range_id.filter(|_| !policy_rejection) {
                        tracing::warn!(
                            ?session_id,
                            position_seconds,
                            ?mode,
                            target_nsecs,
                            range_id,
                            rejection_reason = miss.reason.as_str(),
                            seek_generation,
                            "cached seek target inside advertised range was rejected; retracting range"
                        );
                    } else if let Some(range_id) = miss.range_id {
                        tracing::debug!(
                            ?session_id,
                            position_seconds,
                            ?mode,
                            target_nsecs,
                            range_id,
                            rejection_reason = miss.reason.as_str(),
                            seek_generation,
                            "cached seek range remained valid but did not contain a safe IDR/BLA anchor"
                        );
                    }
                    if low_level_on_miss {
                        guard.request_seek(
                            position_seconds,
                            session_id,
                            seek_generation,
                            target_nsecs,
                        );
                        tracing::debug!(
                            ?session_id,
                            position_seconds,
                            ?mode,
                            target_nsecs,
                            cached_seek_rejection_range_id = ?miss.range_id,
                            cached_seek_rejection_reason = miss.reason.as_str(),
                            seek_generation,
                            generation = guard.generation,
                            cache_lock_wait_ms = cache_lock_wait.as_secs_f64() * 1_000.0,
                            lookup_ms = lookup.as_secs_f64() * 1_000.0,
                            commit_ms = commit.as_secs_f64() * 1_000.0,
                            "FFmpeg demux packet cache seek miss; requested low-level seek"
                        );
                    } else {
                        tracing::debug!(
                            ?session_id,
                            position_seconds,
                            ?mode,
                            target_nsecs,
                            cached_seek_rejection_range_id = ?miss.range_id,
                            cached_seek_rejection_reason = miss.reason.as_str(),
                            seek_generation,
                            generation = guard.generation,
                            cache_lock_wait_ms = cache_lock_wait.as_secs_f64() * 1_000.0,
                            lookup_ms = lookup.as_secs_f64() * 1_000.0,
                            commit_ms = commit.as_secs_f64() * 1_000.0,
                            "FFmpeg demux packet cache-only seek unavailable; low-level seek suppressed"
                        );
                    }
                    let cache_snapshot =
                        guard.cache_report_snapshot(self.shared.control.is_cache_paused());
                    let buffered_changed =
                        guard.take_buffered_changed_for_cache_end(cache_snapshot.cache_end());
                    guard.record_cache_state_emit(Instant::now());
                    guard.record_emitted_seekable_ranges(cache_snapshot.seekable_ranges().clone());
                    self.shared.refresh_monitor_snapshot(&guard);
                    (
                        if low_level_on_miss {
                            DemuxSeekResult::Requested
                        } else {
                            DemuxSeekResult::Unavailable
                        },
                        low_level_on_miss && guard.cache_pause_initial,
                        cache_snapshot,
                        buffered_changed,
                    )
                }
            }
        };
        let cache_state = cache_snapshot.into_cache_state();
        self.shared.notify_ready();
        self.shared
            .send_cache_state_events(session_id, cache_state, buffered_changed);
        if should_enter_initial_cache_pause {
            self.shared.enter_initial_cache_pause_if_needed();
        }
        tracing::debug!(
            ?session_id,
            position_seconds,
            ?mode,
            seek_generation,
            ?result,
            cache_lock_wait_ms = cache_lock_wait.as_secs_f64() * 1_000.0,
            lookup_ms = lookup.as_secs_f64() * 1_000.0,
            total_ms = seek_started_at.elapsed().as_secs_f64() * 1_000.0,
            "completed two-phase FFmpeg demux seek transaction"
        );
        result
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn seek_low_level(
        &self,
        position_seconds: f64,
        session_id: PlaybackSessionId,
        seek_generation: u64,
        reason: &'static str,
    ) -> DemuxSeekResult {
        let position_seconds = position_seconds.max(0.0);
        let target_nsecs = seconds_to_nsecs(position_seconds);
        if self.shared.control.has_pending_seek()
            && self.shared.control.seek_generation() != seek_generation
        {
            tracing::debug!(
                ?session_id,
                position_seconds,
                target_nsecs,
                seek_generation,
                latest_seek_generation = self.shared.control.seek_generation(),
                reason,
                "suppressed superseded forced low-level seek"
            );
            return DemuxSeekResult::Superseded;
        }
        let (cache_snapshot, buffered_changed, should_enter_initial_cache_pause) = {
            let mut guard = self
                .shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.error = None;
            guard.request_seek(position_seconds, session_id, seek_generation, target_nsecs);
            tracing::debug!(
                ?session_id,
                position_seconds,
                target_nsecs,
                seek_generation,
                reason,
                generation = guard.generation,
                "FFmpeg demux packet cache forced low-level seek"
            );
            let cache_snapshot = guard.cache_report_snapshot(self.shared.control.is_cache_paused());
            let buffered_changed =
                guard.take_buffered_changed_for_cache_end(cache_snapshot.cache_end());
            guard.record_cache_state_emit(Instant::now());
            guard.record_emitted_seekable_ranges(cache_snapshot.seekable_ranges().clone());
            self.shared.refresh_monitor_snapshot(&guard);
            (cache_snapshot, buffered_changed, guard.cache_pause_initial)
        };
        let cache_state = cache_snapshot.into_cache_state();
        self.shared.notify_ready();
        self.shared
            .send_cache_state_events(session_id, cache_state, buffered_changed);
        if should_enter_initial_cache_pause {
            self.shared.enter_initial_cache_pause_if_needed();
        }
        DemuxSeekResult::Requested
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn exclude_failed_cached_seek_range(
        &self,
        info: DemuxCachedSeekInfo,
        reason: &'static str,
    ) -> bool {
        let emit = {
            let mut guard = self
                .shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            if !guard.exclude_failed_cached_seek_range(info) {
                tracing::debug!(
                    session_id = ?guard.session_id,
                    range_id = info.range_id,
                    anchor_packet_id = info.anchor_packet_id,
                    anchor_kind = info.anchor_kind.as_str(),
                    reason,
                    "failed cached seek range exclusion did not change authoritative ranges"
                );
                None
            } else {
                let snapshot = guard.cache_report_snapshot(self.shared.control.is_cache_paused());
                let buffered_changed =
                    guard.take_buffered_changed_for_cache_end(snapshot.cache_end());
                guard.record_cache_state_emit(Instant::now());
                guard.record_emitted_seekable_ranges(snapshot.seekable_ranges().clone());
                self.shared.refresh_monitor_snapshot(&guard);
                Some((snapshot, buffered_changed))
            }
        };
        let Some((snapshot, buffered_changed)) = emit else {
            return false;
        };
        let session_id = snapshot.session_id;
        self.shared.send_cache_state_events(
            session_id,
            snapshot.into_cache_state(),
            buffered_changed,
        );
        tracing::warn!(
            ?session_id,
            range_id = info.range_id,
            anchor_packet_id = info.anchor_packet_id,
            anchor_kind = info.anchor_kind.as_str(),
            anchor_nsecs = info.anchor_nsecs,
            target_nsecs = info.target_nsecs,
            preroll_nsecs = info.preroll_nsecs,
            reason,
            "published cached seek ranges after excluding failed recovery anchor"
        );
        true
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn shutdown(&self) {
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.shutdown = true;
        self.shared.notify_ready();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn apply_cache_config(
        &self,
        cache_config: PlaybackCacheConfig,
    ) {
        let emit = {
            let mut guard = self
                .shared
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            let had_cache_buffering = guard.cache_buffering_percent.is_some();
            guard.apply_cache_config(cache_config);
            if !guard.cache_pause_enabled {
                let changed = self.shared.control.is_cache_paused()
                    && self.shared.control.set_cache_paused(false);
                if had_cache_buffering {
                    let _ = self.shared.event_tx.send(BackendEvent::new(
                        guard.session_id,
                        BackendEventKind::CacheBufferingChanged(None),
                    ));
                }
                if changed {
                    let _ = self.shared.event_tx.send(BackendEvent::new(
                        guard.session_id,
                        BackendEventKind::PausedForCacheChanged(false),
                    ));
                    let _ = self.shared.event_tx.send(BackendEvent::new(
                        guard.session_id,
                        BackendEventKind::Pause(self.shared.control.is_paused()),
                    ));
                }
            }
            self.shared.refresh_cache_pause(&mut guard);
            let emit = self.shared.prepare_cache_state_emit(&mut guard);
            self.shared.notify_ready();
            emit
        };
        self.shared.send_cache_state_emit(emit.into_emit());
    }

    pub(in crate::player::backend::ffmpeg::playback_loop) fn wait_until_initial_cache_fill(
        &self,
    ) -> std::result::Result<(), String> {
        let mut guard = self
            .shared
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let wait_started_at = Instant::now();
        let mut next_initial_wait_log_at =
            wait_started_at.checked_add(DEMUX_PACKET_CACHE_STALL_LOG_AFTER);
        loop {
            if guard.shutdown || self.shared.control.should_stop() {
                return Ok(());
            }
            if self.shared.control.has_pending_seek() {
                return Ok(());
            }
            if let Some(error) = guard.error.clone() {
                return Err(error);
            }
            if guard.initial_cache_fill_complete() {
                return Ok(());
            }
            let now = Instant::now();
            if next_initial_wait_log_at.is_some_and(|deadline| now >= deadline) {
                tracing::debug!(
                    session_id = ?guard.session_id,
                    waited_ms = now.saturating_duration_since(wait_started_at).as_millis(),
                    read_index = guard.read_index,
                    packet_count = guard.read_range().global_order.len(),
                    cached_bytes = guard.cached_bytes,
                    forward_bytes = guard.forward_bytes(),
                    forward_duration_ms = guard.forward_duration_nsecs() as f64 / 1_000_000.0,
                    effective_cache_pause_forward_ms =
                        guard.cache_pause_forward_duration_nsecs() as f64 / 1_000_000.0,
                    reader_nsecs = guard.reader_nsecs,
                    exact_seek_target_nsecs = guard.exact_seek_target_nsecs,
                    exact_seek_target_covered = guard.cache_pause_target_covered(),
                    reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                    cached_until_nsecs = ?guard.cached_until_nsecs(),
                    per_stream = ?guard.packet_queue_snapshot().streams,
                    cache_end_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                    raw_input_rate_bytes_per_sec = ?guard.raw_input_rate(),
                    cache_pause_percent = ?guard.cache_pause_percent(),
                    cache_paused = self.shared.control.is_cache_paused(),
                    should_pause_demux = guard.should_pause_demux(),
                    readahead_ms = guard.readahead_nsecs as f64 / 1_000_000.0,
                    cache_pause_wait_ms = guard.cache_pause_wait_nsecs as f64 / 1_000_000.0,
                    "still waiting for initial FFmpeg demux cache fill"
                );
                next_initial_wait_log_at = now.checked_add(DEMUX_PACKET_CACHE_STALL_LOG_INTERVAL);
            }
            guard = self
                .shared
                .wait_for_ready_change(guard, DEMUX_PACKET_CACHE_WAIT_INTERVAL);
        }
    }
}

impl Drop for DemuxPacketCache {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
