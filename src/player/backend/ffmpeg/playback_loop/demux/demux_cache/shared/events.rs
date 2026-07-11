use std::{sync::TryLockError, time::Instant};

use super::{
    BackendEvent, BackendEventKind, CacheStateEmit, DemuxCacheReportSnapshot,
    DemuxPacketAppendTiming, DemuxPacketCacheShared, DemuxPacketCacheState, PlaybackCacheState,
    PlaybackSessionId,
};

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct PreparedCacheStateEmit {
    snapshot: DemuxCacheReportSnapshot,
    buffered_changed: Option<Option<f64>>,
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
        self.send_cache_state_events(emit.session_id, emit.cache_state, emit.buffered_changed);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn prepare_cache_state_emit(
        &self,
        guard: &mut DemuxPacketCacheState,
    ) -> PreparedCacheStateEmit {
        let snapshot = guard.cache_report_snapshot(self.control.is_cache_paused());
        let buffered_changed = guard.take_buffered_changed_for_cache_end(snapshot.cache_end());
        guard.log_seekable_range_diagnostics(
            snapshot.seekable_ranges(),
            usize::try_from(snapshot.demux.forward_bytes).unwrap_or(usize::MAX),
        );
        guard.record_cache_state_emit(Instant::now());
        guard.record_emitted_seekable_ranges(snapshot.seekable_ranges().clone());
        guard.clear_cache_state_emit_dirty();
        self.refresh_monitor_snapshot(guard);
        PreparedCacheStateEmit {
            snapshot,
            buffered_changed,
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
        let (seekable_ranges_changed, seekable_window_contracted) = if appended {
            guard.seekable_range_change_since_last_emit()
        } else {
            (false, false)
        };
        let force = force || seekable_window_contracted;
        if appended || force {
            guard.mark_cache_state_emit_dirty();
        }
        let first_report = guard.last_cache_state_emit_at.is_none();
        let should_emit = guard.cache_state_emit_dirty()
            && (seekable_window_contracted || first_report || guard.cache_state_report_due(now));
        if !should_emit {
            return (None, false, force);
        }
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
        let emit = self.prepare_cache_state_emit(&mut guard);
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
            let emit = self.prepare_cache_state_emit(guard);
            self.send_cache_state_emit(emit.into_emit());
        }
    }
}
