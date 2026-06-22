use std::{sync::TryLockError, time::Instant};

use super::{
    BackendEvent, BackendEventKind, CacheStateEmit, DemuxPacketCacheShared, DemuxPacketCacheState,
    PlaybackCacheState, PlaybackSessionId,
};

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
        now: Instant,
    ) -> CacheStateEmit {
        let cache_state = guard.playback_cache_state(self.control.is_cache_paused());
        let buffered_changed = guard.take_buffered_changed_for_cache_state(&cache_state);
        guard.record_cache_state_emit(now);
        guard.record_emitted_cache_state(&cache_state);
        self.refresh_monitor_snapshot(guard);
        CacheStateEmit {
            session_id: guard.session_id,
            cache_state,
            buffered_changed,
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn prepare_cache_state_emit_after_append(
        &self,
        force: bool,
    ) -> Option<CacheStateEmit> {
        let now = Instant::now();
        let mut guard = if force {
            self.state
                .lock()
                .expect("FFmpeg demux packet cache poisoned")
        } else {
            match self.state.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::WouldBlock) => return None,
                Err(TryLockError::Poisoned(_)) => panic!("FFmpeg demux packet cache poisoned"),
            }
        };
        let first_report = guard.last_cache_state_emit_at.is_none();
        if !force && !first_report && !guard.cache_state_report_due(now) {
            return None;
        }
        Some(self.prepare_cache_state_emit(&mut guard, now))
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn emit_cache_state(
        &self,
        guard: &mut DemuxPacketCacheState,
    ) {
        let emit = self.prepare_cache_state_emit(guard, Instant::now());
        self.send_cache_state_emit(emit);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn emit_cache_state_after_read(
        &self,
        guard: &mut DemuxPacketCacheState,
        force: bool,
    ) {
        let now = Instant::now();
        if !force && !guard.cache_state_report_due(now) {
            return;
        }
        let emit = self.prepare_cache_state_emit(guard, now);
        self.send_cache_state_emit(emit);
    }
}
