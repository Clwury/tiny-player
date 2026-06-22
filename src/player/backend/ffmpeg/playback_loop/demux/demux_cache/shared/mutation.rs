use std::time::Instant;

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
        let mut append_outcome = guard.append_packet(packet);
        append_outcome.timing.lock_wait = append_lock_wait;
        let refresh_cache_pause_started_at = Instant::now();
        let cache_pause_refresh = self.refresh_cache_pause_after_append(&mut guard);
        append_outcome.timing.refresh_cache_pause += refresh_cache_pause_started_at.elapsed();
        let first_cache_state_report = guard.last_cache_state_emit_at.is_none();
        let cache_state_report_due = guard.cache_state_report_due(Instant::now());
        if append_outcome.appended {
            append_outcome.force_cache_state_report |= cache_pause_refresh.force_cache_state_report;
        }
        let force_cache_state_report = append_outcome.force_cache_state_report
            || (!append_outcome.appended && cache_pause_refresh.force_cache_state_report);
        let should_emit_cache_state =
            force_cache_state_report || first_cache_state_report || cache_state_report_due;
        let notify_started_at = Instant::now();
        self.ready.notify_all();
        append_outcome.timing.notify += notify_started_at.elapsed();
        append_outcome.timing.lock_hold = append_lock_hold_started_at.elapsed();
        drop(guard);
        let emit_state_started_at = Instant::now();
        let cache_state_emit = should_emit_cache_state
            .then(|| self.prepare_cache_state_emit_after_append(force_cache_state_report))
            .flatten();
        append_outcome.timing.emit_state += emit_state_started_at.elapsed();
        if let Some(emit) = cache_state_emit {
            self.send_cache_state_emit(emit);
        }
        log_demux_packet_append_timing(
            session_id,
            packet_stream_index,
            packet_bytes,
            append_outcome,
            cache_pause_refresh.force_cache_state_report,
        );
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn mark_eof(&self) {
        let mut guard = self
            .state
            .lock()
            .expect("FFmpeg demux packet cache poisoned");
        guard.mark_eof();
        self.refresh_cache_pause(&mut guard);
        self.emit_cache_state(&mut guard);
        self.ready.notify_all();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn set_error(
        &self,
        error: String,
    ) {
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
        self.emit_cache_state(&mut guard);
        self.ready.notify_all();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn clear_cache_pause_for_decoded_resume(
        &self,
    ) {
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
        if changed || had_percent {
            self.emit_cache_state(&mut guard);
            self.ready.notify_all();
        }
    }
}
