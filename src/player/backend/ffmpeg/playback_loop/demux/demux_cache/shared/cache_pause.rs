use super::{
    BackendEvent, BackendEventKind, CachePauseRefresh, DemuxPacketCacheShared,
    DemuxPacketCacheState, nsecs_to_seconds,
};

impl DemuxPacketCacheShared {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn enter_initial_cache_pause_if_needed(
        &self,
    ) {
        let emit = {
            let mut guard = self
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            if guard.cache_pause_initial && guard.cache_pause_can_enter(false) {
                self.enter_cache_pause(&mut guard);
            }
            self.prepare_cache_state_emit(&mut guard)
        };
        self.send_cache_state_emit(emit.into_emit());
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn enter_cache_pause_if_needed(
        &self,
        guard: &mut DemuxPacketCacheState,
        cache_pause_signal: bool,
    ) {
        if !cache_pause_signal || !guard.cache_pause_can_enter(true) {
            return;
        }
        if self.enter_cache_pause(guard).force_cache_state_report {
            guard.mark_cache_state_emit_dirty();
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn enter_cache_pause(
        &self,
        guard: &mut DemuxPacketCacheState,
    ) -> CachePauseRefresh {
        let changed = self.control.set_cache_paused(true);
        let percent = guard.cache_pause_percent();
        if changed {
            tracing::debug!(
                session_id = ?guard.session_id,
                buffering_percent = ?percent,
                read_index = guard.read_index,
                packet_count = guard.read_range().global_order.len(),
                cached_bytes = guard.cached_bytes,
                forward_bytes = guard.forward_bytes(),
                forward_duration_ms = guard.forward_duration_nsecs() as f64 / 1_000_000.0,
                reader_nsecs = guard.reader_nsecs,
                reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                cached_until_nsecs = ?guard.cached_until_nsecs(),
                cache_end_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                raw_input_rate_bytes_per_sec = ?guard.raw_input_rate(),
                should_pause_demux = guard.should_pause_demux(),
                readahead_ms = guard.readahead_nsecs as f64 / 1_000_000.0,
                cache_pause_wait_ms = guard.cache_pause_wait_nsecs as f64 / 1_000_000.0,
                "FFmpeg demux packet cache pause entered"
            );
        }
        if changed {
            let _ = self.event_tx.send(BackendEvent::new(
                guard.session_id,
                BackendEventKind::PausedForCacheChanged(true),
            ));
        }
        if guard.cache_buffering_percent != percent {
            guard.cache_buffering_percent = percent;
            let _ = self.event_tx.send(BackendEvent::new(
                guard.session_id,
                BackendEventKind::CacheBufferingChanged(percent),
            ));
        }
        if changed {
            let _ = self.event_tx.send(BackendEvent::new(
                guard.session_id,
                BackendEventKind::Pause(self.control.is_paused()),
            ));
        }
        CachePauseRefresh {
            force_cache_state_report: changed,
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn refresh_cache_pause(
        &self,
        guard: &mut DemuxPacketCacheState,
    ) -> CachePauseRefresh {
        if !self.control.is_cache_paused() {
            if guard.cache_buffering_percent.is_some() {
                guard.cache_buffering_percent = None;
                let _ = self.event_tx.send(BackendEvent::new(
                    guard.session_id,
                    BackendEventKind::CacheBufferingChanged(None),
                ));
                return CachePauseRefresh {
                    force_cache_state_report: true,
                };
            }
            return CachePauseRefresh::default();
        }

        if guard.cache_pause_recovered() {
            let had_percent = guard.cache_buffering_percent.take().is_some();
            let changed = self.control.set_cache_paused(false);
            if changed {
                tracing::debug!(
                    session_id = ?guard.session_id,
                    read_index = guard.read_index,
                    packet_count = guard.read_range().global_order.len(),
                    cached_bytes = guard.cached_bytes,
                    forward_bytes = guard.forward_bytes(),
                    forward_duration_ms = guard.forward_duration_nsecs() as f64 / 1_000_000.0,
                    reader_nsecs = guard.reader_nsecs,
                    reader_pts_seconds = nsecs_to_seconds(guard.reader_nsecs),
                    cached_until_nsecs = ?guard.cached_until_nsecs(),
                    cache_end_seconds = ?guard.cached_until_nsecs().map(nsecs_to_seconds),
                    raw_input_rate_bytes_per_sec = ?guard.raw_input_rate(),
                    should_pause_demux = guard.should_pause_demux(),
                    readahead_ms = guard.readahead_nsecs as f64 / 1_000_000.0,
                    cache_pause_wait_ms = guard.cache_pause_wait_nsecs as f64 / 1_000_000.0,
                    "FFmpeg demux packet cache pause recovered"
                );
            }
            if had_percent {
                let _ = self.event_tx.send(BackendEvent::new(
                    guard.session_id,
                    BackendEventKind::CacheBufferingChanged(None),
                ));
            }
            if changed {
                let _ = self.event_tx.send(BackendEvent::new(
                    guard.session_id,
                    BackendEventKind::PausedForCacheChanged(false),
                ));
                let _ = self.event_tx.send(BackendEvent::new(
                    guard.session_id,
                    BackendEventKind::Pause(self.control.is_paused()),
                ));
            }
            return CachePauseRefresh {
                force_cache_state_report: had_percent || changed,
            };
        }

        let percent = guard.cache_pause_percent();
        if guard.cache_buffering_percent != percent {
            guard.cache_buffering_percent = percent;
            let _ = self.event_tx.send(BackendEvent::new(
                guard.session_id,
                BackendEventKind::CacheBufferingChanged(percent),
            ));
        }
        CachePauseRefresh::default()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn refresh_cache_pause_after_append(
        &self,
        guard: &mut DemuxPacketCacheState,
    ) -> CachePauseRefresh {
        if !self.control.is_cache_paused() && guard.cache_buffering_percent.is_none() {
            return CachePauseRefresh::default();
        }
        self.refresh_cache_pause(guard)
    }
}
