use std::time::Instant;

use super::{
    ByteRingBuffer, HTTP_CACHE_STALL_LOG_INTERVAL, HttpCacheRangeKind, HttpCacheReadError,
    HttpReadWaitLogDecision, HttpReadWaitLogState, HttpReadWaitObservation, HttpReadWaitPosition,
    HttpRingCacheState, RetainedCacheRange,
};

impl HttpRingCacheState {
    pub(in crate::player::backend::ffmpeg::avio::cache) fn read_wait_observation(
        &self,
        current_offset: u64,
        output_backpressure_paused: bool,
    ) -> HttpReadWaitObservation {
        let position = if current_offset < self.base_offset {
            HttpReadWaitPosition::BeforeActiveRange
        } else if current_offset < self.next_offset {
            HttpReadWaitPosition::WithinActiveRange
        } else if current_offset == self.next_offset {
            HttpReadWaitPosition::AtAppendEdge
        } else {
            HttpReadWaitPosition::AheadOfActiveRange
        };
        HttpReadWaitObservation {
            position,
            active_range_kind: self.active_range_kind,
            prefetch_paused: self.prefetch_paused,
            output_backpressure_paused,
            restart_pending: self.restart_request.is_some(),
            side_download_pending: self.side_download_may_produce(current_offset),
            eof: self.eof,
        }
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn observe_read_wait_log(
        &mut self,
        observation: HttpReadWaitObservation,
        now: Instant,
    ) -> HttpReadWaitLogDecision {
        let Some(state) = self.read_wait_log_state.as_mut() else {
            self.read_wait_log_state = Some(HttpReadWaitLogState {
                observation,
                started_at: now,
                last_logged_at: now,
                suppressed_repeats: 0,
            });
            return HttpReadWaitLogDecision::Changed {
                suppressed_repeats: 0,
            };
        };
        if state.observation != observation {
            let suppressed_repeats = state.suppressed_repeats;
            *state = HttpReadWaitLogState {
                observation,
                started_at: now,
                last_logged_at: now,
                suppressed_repeats: 0,
            };
            return HttpReadWaitLogDecision::Changed { suppressed_repeats };
        }

        state.suppressed_repeats = state.suppressed_repeats.saturating_add(1);
        if now.saturating_duration_since(state.last_logged_at) >= HTTP_CACHE_STALL_LOG_INTERVAL {
            let repeated_observations = std::mem::take(&mut state.suppressed_repeats);
            state.last_logged_at = now;
            HttpReadWaitLogDecision::Summary {
                repeated_observations,
                blocked_for: now.saturating_duration_since(state.started_at),
            }
        } else {
            HttpReadWaitLogDecision::Suppressed
        }
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn set_read_error(
        &mut self,
        offset: u64,
        message: String,
    ) {
        self.error = Some(HttpCacheReadError { offset, message });
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn read_error_at(
        &self,
        offset: u64,
    ) -> Option<&HttpCacheReadError> {
        self.error.as_ref().filter(|error| error.offset == offset)
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn clear_read_error_covered_by(
        &mut self,
        offset: u64,
        len: usize,
    ) {
        let end = offset.saturating_add(len as u64);
        if self
            .error
            .as_ref()
            .is_some_and(|error| error.offset >= offset && error.offset < end)
        {
            self.error = None;
        }
    }

    pub(in crate::player::backend::ffmpeg) fn copy_available(
        &mut self,
        offset: u64,
        output: &mut [u8],
    ) -> Option<usize> {
        if let Some(read) = copy_available_from_range(
            &self.buffer,
            self.base_offset,
            self.next_offset,
            offset,
            output,
        ) {
            return Some(read);
        }
        for index in (0..self.retained_ranges.len()).rev() {
            let Some(read) = self.retained_ranges.get(index).and_then(|range| {
                copy_available_from_range(
                    &range.buffer,
                    range.base_offset,
                    range.next_offset,
                    offset,
                    output,
                )
            }) else {
                continue;
            };
            let generation = self.next_retained_access_generation();
            if let Some(range) = self.retained_ranges.get_mut(index) {
                range.last_used_generation = generation;
            }
            return Some(read);
        }
        self.disk_cache
            .as_mut()
            .and_then(|disk_cache| disk_cache.read_at(offset, output))
    }

    pub(in crate::player::backend::ffmpeg) fn set_reader_offset(&mut self, offset: u64) {
        self.reader_offset = offset;
        if self.offset_in_active_range(offset) {
            self.trim_to_capacity(self.active_memory_capacity());
        }
    }

    pub(in crate::player::backend::ffmpeg) fn note_seek_offset(
        &mut self,
        offset: u64,
        range_kind: HttpCacheRangeKind,
    ) {
        let offset_in_active_range = self.offset_in_active_range(offset);
        let offset_cached = self.cached_range_contains(offset);
        if !offset_in_active_range {
            self.byte_level_seeks = self.byte_level_seeks.saturating_add(1);
        }
        self.pending_seek_range_kind = Some((offset, range_kind));
        if range_kind == HttpCacheRangeKind::Playback {
            self.restart_request = None;
            if !offset_in_active_range {
                self.demote_active_range_to_retained();
            }
            self.set_reader_offset(offset);
            if !offset_cached {
                self.request_active_playback_restart_at(offset);
            }
        }
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn demote_active_range_to_retained(
        &mut self,
    ) {
        if self.buffer.len() == 0 || self.next_offset <= self.base_offset {
            self.base_offset = self.next_offset;
            return;
        }

        tracing::debug!(
            base_offset = self.base_offset,
            next_offset = self.next_offset,
            active_range_kind = ?self.active_range_kind,
            "demoting inactive HTTP active range to retained cache range"
        );
        let capacity = self.buffer.max_capacity();
        let buffer = std::mem::replace(&mut self.buffer, ByteRingBuffer::new(capacity));
        let last_used_generation = self.next_retained_access_generation();
        self.retained_ranges.push_back(RetainedCacheRange {
            buffer,
            base_offset: self.base_offset,
            next_offset: self.next_offset,
            range_kind: self.active_range_kind,
            last_used_generation,
        });
        self.base_offset = self.next_offset;
        self.trim_retained_ranges_to_capacity(self.config.memory_capacity);
    }
}

pub(in crate::player::backend::ffmpeg::avio::cache) fn copy_available_from_range(
    buffer: &ByteRingBuffer,
    base_offset: u64,
    next_offset: u64,
    offset: u64,
    output: &mut [u8],
) -> Option<usize> {
    if offset < base_offset || offset >= next_offset {
        return None;
    }
    let start = usize::try_from(offset - base_offset).ok()?;
    if start >= buffer.len() {
        return None;
    }
    let read = buffer.copy_at(start, output);
    (read > 0).then_some(read)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{HttpReadWaitLogDecision, HttpRingCacheState};

    #[test]
    fn identical_http_read_waits_log_immediately_then_once_per_second() {
        let mut state = HttpRingCacheState::new(0);
        let now = Instant::now();
        let observation = state.read_wait_observation(0, false);

        assert_eq!(
            state.observe_read_wait_log(observation, now),
            HttpReadWaitLogDecision::Changed {
                suppressed_repeats: 0,
            }
        );
        assert_eq!(
            state.observe_read_wait_log(observation, now + Duration::from_millis(10)),
            HttpReadWaitLogDecision::Suppressed
        );
        assert_eq!(
            state.observe_read_wait_log(observation, now + Duration::from_secs(1)),
            HttpReadWaitLogDecision::Summary {
                repeated_observations: 2,
                blocked_for: Duration::from_secs(1),
            }
        );

        state.prefetch_paused = true;
        let changed = state.read_wait_observation(0, false);
        assert!(matches!(
            state.observe_read_wait_log(changed, now + Duration::from_millis(1_010)),
            HttpReadWaitLogDecision::Changed { .. }
        ));
    }
}
