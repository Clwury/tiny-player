use std::{sync::TryLockError, time::Instant};

use super::{
    BackendEvent, BackendEventKind, CacheStateEmit, DemuxCacheReportSnapshot,
    DemuxPacketAppendTiming, DemuxPacketCacheShared, DemuxPacketCacheState, PlaybackCacheState,
    PlaybackSessionId,
};

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct PreparedCacheStateEmit {
    snapshot: DemuxCacheReportSnapshot,
    buffered_changed: Option<Option<f64>>,
    notify_cache_state: bool,
}

impl PreparedCacheStateEmit {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn into_emit(
        self,
    ) -> CacheStateEmit {
        let session_id = self.snapshot.session_id;
        CacheStateEmit {
            session_id,
            cache_state: self.snapshot.into_cache_state(),
            buffered_changed: self.buffered_changed,
            notify_cache_state: self.notify_cache_state,
        }
    }
}

impl DemuxPacketCacheShared {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn send_cache_state_events(
        &self,
        session_id: PlaybackSessionId,
        cache_state: PlaybackCacheState,
        buffered_changed: Option<Option<f64>>,
    ) {
        let _ = self.event_tx.send(BackendEvent::new(
            session_id,
            BackendEventKind::CacheStateChanged(cache_state),
        ));
        if let Some(buffered_until) = buffered_changed {
            let _ = self.event_tx.send(BackendEvent::new(
                session_id,
                BackendEventKind::BufferedChanged(buffered_until),
            ));
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn send_cache_state_emit(
        &self,
        emit: CacheStateEmit,
    ) {
        if emit.notify_cache_state {
            let _ = self.event_tx.send(BackendEvent::new(
                emit.session_id,
                BackendEventKind::CacheStateChanged(emit.cache_state),
            ));
        }
        if let Some(buffered_until) = emit.buffered_changed {
            let _ = self.event_tx.send(BackendEvent::new(
                emit.session_id,
                BackendEventKind::BufferedChanged(buffered_until),
            ));
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn prepare_cache_state_emit(
        &self,
        guard: &mut DemuxPacketCacheState,
    ) -> PreparedCacheStateEmit {
        self.prepare_cache_state_emit_for(guard, "control", true)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn prepare_cache_state_emit_for(
        &self,
        guard: &mut DemuxPacketCacheState,
        emit_thread: &'static str,
        force_notify: bool,
    ) -> PreparedCacheStateEmit {
        let prepare_started_at = Instant::now();
        let snapshot = guard.cache_report_snapshot(self.control.is_cache_paused());
        self.prepare_cache_state_emit_from_snapshot(
            guard,
            snapshot,
            emit_thread,
            force_notify,
            prepare_started_at,
        )
    }

    fn prepare_cache_state_emit_from_prepared_for(
        &self,
        guard: &mut DemuxPacketCacheState,
        emit_thread: &'static str,
        force_notify: bool,
    ) -> Option<PreparedCacheStateEmit> {
        let prepare_started_at = Instant::now();
        let snapshot = guard.cache_report_snapshot_from_prepared(self.control.is_cache_paused())?;
        Some(self.prepare_cache_state_emit_from_snapshot(
            guard,
            snapshot,
            emit_thread,
            force_notify,
            prepare_started_at,
        ))
    }

    fn prepare_cache_state_emit_from_snapshot(
        &self,
        guard: &mut DemuxPacketCacheState,
        snapshot: DemuxCacheReportSnapshot,
        emit_thread: &'static str,
        force_notify: bool,
        prepare_started_at: Instant,
    ) -> PreparedCacheStateEmit {
        let buffered_changed = guard.take_buffered_changed_for_cache_end(snapshot.cache_end());
        let normalized_ranges_changed =
            guard.normalized_seekable_ranges_changed(snapshot.seekable_ranges());
        let notify_cache_state = force_notify
            || normalized_ranges_changed
            || guard.demux_cache_state_changed(&snapshot.demux);
        let cache_lock_hold = prepare_started_at.elapsed();
        guard.log_seekable_range_diagnostics(
            &snapshot,
            emit_thread,
            cache_lock_hold,
            normalized_ranges_changed,
        );
        guard.record_cache_state_emit(Instant::now());
        guard.record_emitted_seekable_ranges_at_revision(
            snapshot.seekable_ranges().clone(),
            snapshot.seekability_revision,
        );
        guard.record_emitted_demux_cache_state(snapshot.demux.clone());
        if snapshot.seekability_revision == guard.seekability_revision() {
            guard.clear_cache_state_emit_dirty();
        } else {
            // Reader/underrun state was published with an older prepared range
            // snapshot. Keep the producer maintenance request alive until it
            // publishes the current seekability revision.
            guard.mark_cache_state_emit_dirty();
        }
        self.refresh_monitor_snapshot(guard);
        PreparedCacheStateEmit {
            snapshot,
            buffered_changed,
            notify_cache_state,
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn prepare_cache_state_emit_after_append_with_timing(
        &self,
        force: bool,
        appended: bool,
        defer_for_consumer: bool,
        timing: &mut DemuxPacketAppendTiming,
    ) -> (Option<CacheStateEmit>, bool, bool) {
        let now = Instant::now();
        let lock_started_at = Instant::now();
        let mut guard = match self.state.try_lock() {
            Ok(guard) => {
                timing.emit_state_lock_wait += lock_started_at.elapsed();
                guard
            }
            Err(TryLockError::WouldBlock) => {
                timing.emit_state_lock_wait += lock_started_at.elapsed();
                return (None, false, force);
            }
            Err(TryLockError::Poisoned(_)) => panic!("FFmpeg demux packet cache poisoned"),
        };
        if appended || force {
            guard.mark_cache_state_emit_dirty();
        }
        let first_report = guard.last_cache_state_emit_at.is_none();
        let should_emit =
            guard.cache_state_emit_dirty() && (first_report || guard.cache_state_report_due(now));
        if !should_emit {
            return (None, false, force);
        }
        let seekable_ranges_changed = appended && guard.seekable_ranges_changed_since_last_emit();
        if defer_for_consumer
            && !force
            && !first_report
            && !seekable_ranges_changed
            && guard.consumer_readable_packet_available()
        {
            tracing::trace!(
                session_id = ?guard.session_id,
                "deferred FFmpeg demux cache state emit while consumer-readable packets are available"
            );
            return (None, true, force);
        }
        let prepare_started_at = Instant::now();
        let emit = self.prepare_cache_state_emit_for(&mut guard, "prefetch", force || first_report);
        timing.emit_state_prepare += prepare_started_at.elapsed();
        drop(guard);
        (Some(emit.into_emit()), false, force)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn emit_cache_state_after_read(
        &self,
        guard: &mut DemuxPacketCacheState,
        force: bool,
    ) {
        guard.mark_cache_state_emit_dirty();
        if force || guard.cache_state_report_due(Instant::now()) {
            let Some(emit) =
                self.prepare_cache_state_emit_from_prepared_for(guard, "consumer", force)
            else {
                tracing::trace!(
                    session_id = ?guard.session_id,
                    seekability_revision = guard.seekability_revision(),
                    force,
                    emit_thread = "consumer",
                    "deferred FFmpeg demux cache state emit until seekable summary is prepared"
                );
                return;
            };
            self.send_cache_state_emit(emit.into_emit());
        }
    }
}
