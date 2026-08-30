use std::{sync::TryLockError, time::Instant};

use super::super::DemuxPacketDiskCache;
use super::{
    BackendEvent, BackendEventKind, CachedDemuxPacket, DemuxPacketCacheShared,
    log_demux_packet_append_timing,
};

impl DemuxPacketCacheShared {
    /// Spill packet payload bytes without holding the cache mutex during file
    /// I/O. Space is reserved under the mutex, then the write happens through
    /// the shared file handle and the packet is converted to a disk payload
    /// before it is inserted into the queue.
    fn spill_packet_outside_state_lock(
        &self,
        packet: &mut CachedDemuxPacket,
    ) -> std::time::Duration {
        let started_at = Instant::now();
        let disk_enabled = {
            let guard = self
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.disk_cache_writable && guard.disk_cache.is_some()
        };
        if !disk_enabled {
            return started_at.elapsed();
        }

        let spill = match packet.prepare_disk_spill() {
            Ok(Some(spill)) => spill,
            Ok(None) => return started_at.elapsed(),
            Err(error) => {
                tracing::warn!(%error, "pausing FFmpeg demux packet disk cache writes");
                if let Ok(mut guard) = self.state.lock() {
                    guard.disk_cache_writable = false;
                }
                return started_at.elapsed();
            }
        };
        let reservation = {
            let mut guard = self
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            if !guard.disk_cache_writable {
                None
            } else if let Some(disk_cache) = guard.disk_cache.as_mut() {
                match disk_cache.reserve_packet(spill.data.len()) {
                    Ok((offset, file)) => Some((offset, file)),
                    Err(error) => {
                        tracing::warn!(%error, "pausing FFmpeg demux packet disk cache writes");
                        guard.disk_cache_writable = false;
                        None
                    }
                }
            } else {
                None
            }
        };
        let Some((offset, file)) = reservation else {
            return started_at.elapsed();
        };

        if let Err(error) = DemuxPacketDiskCache::write_reserved_packet(&file, offset, &spill.data)
        {
            tracing::warn!(%error, "pausing FFmpeg demux packet disk cache writes");
            if let Ok(mut guard) = self.state.lock() {
                guard.disk_cache_writable = false;
            }
            return started_at.elapsed();
        }
        packet.finish_disk_spill(offset, spill);
        started_at.elapsed()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn note_producer_recovering(
        &self,
        error: String,
        consecutive_errors: u32,
    ) -> (bool, u64) {
        let mut guard = self
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.producer_recovery_error = Some(error);
        guard.producer_recovery_consecutive_errors = consecutive_errors;
        let consumer_drainable = guard.consumer_drainable_packet_available();
        let forward_duration_nsecs = guard.forward_duration_nsecs();
        self.notify_ready();
        (consumer_drainable, forward_duration_nsecs)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn clear_producer_recovery(
        &self,
    ) {
        let mut guard = self
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        if guard.producer_recovery_error.take().is_none()
            && guard.producer_recovery_consecutive_errors == 0
        {
            return;
        }
        guard.producer_recovery_consecutive_errors = 0;
        self.notify_ready();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn append_packet(
        &self,
        mut packet: CachedDemuxPacket,
    ) {
        // Capture the input generation before the potentially slow disk
        // preparation. A seek can replace the active range while the packet
        // is being serialized; stale packets must not be appended to the new
        // generation once the state lock is reacquired.
        let expected_demux_input_generation = {
            let guard = self
                .state
                .lock()
                .expect("FFmpeg demux packet cache poisoned");
            guard.demux_input_generation
        };
        let expected_seek_generation = self.control.seek_generation();
        let packet_stream_index = packet.stream_index;
        let packet_bytes = packet.byte_len;
        let disk_write_elapsed = self.spill_packet_outside_state_lock(&mut packet);
        let lock_wait_started_at = Instant::now();
        let mut guard = self
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        let append_lock_wait = lock_wait_started_at.elapsed();
        if guard.demux_input_generation != expected_demux_input_generation
            || self.control.seek_generation() != expected_seek_generation
        {
            tracing::debug!(
                expected_demux_input_generation,
                current_demux_input_generation = guard.demux_input_generation,
                expected_seek_generation,
                current_seek_generation = self.control.seek_generation(),
                "discarding stale FFmpeg demux packet after cache generation changed"
            );
            return;
        }
        let append_lock_hold_started_at = Instant::now();
        let session_id = guard.session_id;
        let mut append_outcome = guard.append_packet_fast(packet);
        append_outcome.timing.disk_write += disk_write_elapsed;
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
        self.notify_ready();
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
                            self.notify_ready();
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
            self.notify_ready();
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
            guard.producer_recovery_error = None;
            guard.producer_recovery_consecutive_errors = 0;
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
            self.notify_ready();
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
                self.notify_ready();
                emit
            })
        };
        if let Some(emit) = emit {
            self.send_cache_state_emit(emit.into_emit());
        }
    }
}
