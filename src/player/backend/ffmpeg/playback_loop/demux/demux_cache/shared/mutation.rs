use std::{sync::TryLockError, time::Instant};

use super::{
    BackendEvent, BackendEventKind, CachedDemuxPacket, DemuxPacketCacheShared,
    log_demux_packet_append_timing,
};

impl DemuxPacketCacheShared {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn append_packet(
        &self,
        packet: CachedDemuxPacket,
    ) {
        let packet_stream_index = packet.stream_index;
        let packet_bytes = packet.byte_len;
        let lock_wait_started_at = Instant::now();
        let mut guard = self
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let append_lock_wait = lock_wait_started_at.elapsed();
        let append_lock_hold_started_at = Instant::now();
        let session_id = guard.session_id;
        let mut append_outcome = guard.append_packet_fast(packet);
        append_outcome.timing.lock_wait = append_lock_wait;
        let refresh_cache_pause_started_at = Instant::now();
        let cache_pause_refresh = self.refresh_cache_pause_after_append(&mut guard);
        let mut cache_pause_changed = cache_pause_refresh.force_cache_state_report;
        append_outcome.timing.refresh_cache_pause += refresh_cache_pause_started_at.elapsed();
        if append_outcome.appended {
            append_outcome.force_cache_state_report |= cache_pause_changed;
        }
        let mut force_cache_state_report = append_outcome.force_cache_state_report
            || (!append_outcome.appended && cache_pause_changed);
        if append_outcome.appended || force_cache_state_report {
            guard.mark_cache_state_emit_dirty();
        }
        let notify_started_at = Instant::now();
        self.ready.notify_all();
        append_outcome.timing.notify += notify_started_at.elapsed();
        append_outcome.timing.lock_hold += append_lock_hold_started_at.elapsed();
        drop(guard);

        if append_outcome.trim_requested {
            if self.playback_recovery_critical() {
                append_outcome.trim_deferred_for_recovery = true;
                append_outcome.trim_deferred_for_consumer = true;
            } else if self.consumer_priority_active() {
                append_outcome.trim_deferred_for_consumer = true;
            } else {
                let maintenance_lock_started_at = Instant::now();
                match self.state.try_lock() {
                    Ok(mut guard) => {
                        append_outcome.timing.lock_wait += maintenance_lock_started_at.elapsed();
                        if self.playback_recovery_critical() {
                            append_outcome.trim_deferred_for_recovery = true;
                            append_outcome.trim_deferred_for_consumer = true;
                        } else if self.consumer_priority_active() {
                            append_outcome.trim_deferred_for_consumer = true;
                        } else {
                            let maintenance_hold_started_at = Instant::now();
                            let pruned = guard.complete_append_packet_trim(&mut append_outcome);
                            if pruned {
                                let refresh_cache_pause_started_at = Instant::now();
                                let refresh = self.refresh_cache_pause_after_append(&mut guard);
                                append_outcome.timing.refresh_cache_pause +=
                                    refresh_cache_pause_started_at.elapsed();
                                cache_pause_changed |= refresh.force_cache_state_report;
                                append_outcome.force_cache_state_report |=
                                    refresh.force_cache_state_report;
                                force_cache_state_report |= refresh.force_cache_state_report;
                                guard.mark_cache_state_emit_dirty();
                                self.refresh_monitor_snapshot(&guard);
                            }
                            let notify_started_at = Instant::now();
                            self.ready.notify_all();
                            append_outcome.timing.notify += notify_started_at.elapsed();
                            append_outcome.timing.lock_hold +=
                                maintenance_hold_started_at.elapsed();
                        }
                    }
                    Err(TryLockError::WouldBlock) => {
                        append_outcome.timing.lock_wait += maintenance_lock_started_at.elapsed();
                        append_outcome.trim_deferred_for_consumer = true;
                    }
                    Err(TryLockError::Poisoned(_)) => {
                        panic!("FFmpeg demux packet cache poisoned")
                    }
                }
            }
        }

        let emit_state_started_at = Instant::now();
        let (cache_state_emit, cache_state_emit_deferred_for_consumer, force_cache_state_report) =
            self.prepare_cache_state_emit_after_append_with_timing(
                force_cache_state_report,
                append_outcome.appended,
                true,
                &mut append_outcome.timing,
            );
        append_outcome.force_cache_state_report = force_cache_state_report;
        append_outcome.cache_state_emit_deferred_for_consumer =
            cache_state_emit_deferred_for_consumer;
        append_outcome.timing.emit_state += emit_state_started_at.elapsed();
        if let Some(emit) = cache_state_emit {
            let send_started_at = Instant::now();
            self.send_cache_state_emit(emit);
            append_outcome.timing.emit_state_send += send_started_at.elapsed();
        }
        log_demux_packet_append_timing(
            session_id,
            packet_stream_index,
            packet_bytes,
            append_outcome,
            cache_pause_changed,
        );
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn mark_eof(&self) {
        let emit = {
            let mut guard = self
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.mark_eof();
            self.refresh_cache_pause(&mut guard);
            let emit = self.prepare_cache_state_emit(&mut guard);
            self.ready.notify_all();
            emit
        };
        self.send_cache_state_emit(emit.into_emit());
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn set_error(
        &self,
        error: String,
    ) {
        let emit = {
            let mut guard = self
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.error = Some(error);
            guard.seeking = false;
            guard.cache_buffering_percent = None;
            if self.control.set_cache_paused(false) {
                let _ = self.event_tx.send(BackendEvent::new(
                    guard.session_id,
                    BackendEventKind::CacheBufferingChanged(None),
                ));
                let _ = self.event_tx.send(BackendEvent::new(
                    guard.session_id,
                    BackendEventKind::PausedForCacheChanged(false),
                ));
                let _ = self.event_tx.send(BackendEvent::new(
                    guard.session_id,
                    BackendEventKind::Pause(self.control.is_paused()),
                ));
            }
            let emit = self.prepare_cache_state_emit(&mut guard);
            self.ready.notify_all();
            emit
        };
        self.send_cache_state_emit(emit.into_emit());
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn clear_cache_pause_for_decoded_resume(
        &self,
    ) {
        let emit = {
            let mut guard = self
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            let had_percent = guard.cache_buffering_percent.take().is_some();
            let changed = self.control.set_cache_paused(false);
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
            (changed || had_percent).then(|| {
                let emit = self.prepare_cache_state_emit(&mut guard);
                self.ready.notify_all();
                emit
            })
        };
        if let Some(emit) = emit {
            self.send_cache_state_emit(emit.into_emit());
        }
    }
}
