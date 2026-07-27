use super::{
    AUDIO_OUTPUT_QUEUE_LIMIT_DURATION, AUDIO_QUEUE_WAIT_LOG_AFTER, Arc, AtomicBool, AudioBuffer,
    AudioQueueInFlight, AudioQueueItem, AudioQueueShared, AudioQueueSnapshot, AudioQueueState,
    AudioQueueWriteError, AudioQueueWriteProgress, AudioShared, AudioTimelineState, Condvar,
    Duration, FfmpegControl, Instant, JoinHandle, Mutex, Ordering, SCHEDULER_POLL_INTERVAL,
    TryLockError, VecDeque, duration_nsecs, interpolated_audio_timeline_nsecs,
    log_audio_queue_snapshot_timing, thread,
};

impl AudioBuffer {
    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn with_capacity(max_samples: usize) -> Self {
        Self::with_capacity_and_epoch(max_samples, 0)
    }

    pub(in crate::player::backend::ffmpeg::audio) fn with_capacity_and_epoch(
        max_samples: usize,
        epoch: u64,
    ) -> Self {
        Self {
            samples: vec![0.0; max_samples],
            read_pos: 0,
            write_pos: 0,
            len: 0,
            epoch,
        }
    }

    pub(in crate::player::backend::ffmpeg) fn len(&self) -> usize {
        self.len
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(in crate::player::backend::ffmpeg) fn available_capacity(&self) -> usize {
        self.samples.len().saturating_sub(self.len)
    }

    pub(in crate::player::backend::ffmpeg) fn clear(&mut self) {
        self.read_pos = 0;
        self.write_pos = 0;
        self.len = 0;
    }

    pub(in crate::player::backend::ffmpeg) fn push_slice(&mut self, samples: &[f32]) -> usize {
        let writable = samples.len().min(self.available_capacity());
        if writable == 0 || self.samples.is_empty() {
            return 0;
        }

        let first = writable.min(self.samples.len() - self.write_pos);
        self.samples[self.write_pos..self.write_pos + first].copy_from_slice(&samples[..first]);
        self.write_pos = (self.write_pos + first) % self.samples.len();
        self.len += first;

        let remaining = writable - first;
        if remaining > 0 {
            self.samples[..remaining].copy_from_slice(&samples[first..first + remaining]);
            self.write_pos = remaining;
            self.len += remaining;
        }

        writable
    }

    pub(in crate::player::backend::ffmpeg) fn pop_sample(&mut self) -> Option<f32> {
        if self.len == 0 || self.samples.is_empty() {
            return None;
        }
        let sample = self.samples[self.read_pos];
        self.read_pos = (self.read_pos + 1) % self.samples.len();
        self.len -= 1;
        Some(sample)
    }
}

impl AudioQueueState {
    pub(in crate::player::backend::ffmpeg::audio) fn new() -> Self {
        Self {
            items: VecDeque::new(),
            queued_samples: 0,
            queued_duration_nsecs: 0,
            in_flight: None,
        }
    }

    pub(in crate::player::backend::ffmpeg::audio) fn can_accept(
        &self,
        additional_duration_nsecs: u64,
    ) -> bool {
        self.queued_duration_nsecs
            .saturating_add(additional_duration_nsecs)
            <= duration_nsecs(AUDIO_OUTPUT_QUEUE_LIMIT_DURATION)
    }

    pub(in crate::player::backend::ffmpeg::audio) fn push(&mut self, item: AudioQueueItem) {
        self.queued_samples = self.queued_samples.saturating_add(item.samples.len());
        self.queued_duration_nsecs = self
            .queued_duration_nsecs
            .saturating_add(item.duration_nsecs);
        self.items.push_back(item);
    }

    pub(in crate::player::backend::ffmpeg::audio) fn finish_item(
        &mut self,
        samples: usize,
        duration_nsecs: u64,
    ) {
        self.queued_samples = self.queued_samples.saturating_sub(samples);
        self.queued_duration_nsecs = self.queued_duration_nsecs.saturating_sub(duration_nsecs);
    }

    pub(in crate::player::backend::ffmpeg::audio) fn clear(&mut self) {
        self.items.clear();
        self.queued_samples = 0;
        self.queued_duration_nsecs = 0;
        self.in_flight = None;
    }

    pub(in crate::player::backend::ffmpeg::audio) fn pending_duration(&self) -> Duration {
        Duration::from_nanos(self.queued_duration_nsecs)
    }
}

impl AudioQueueShared {
    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::audio) fn new(control: Arc<FfmpegControl>) -> Self {
        Self::with_timeline(control, Arc::new(AudioTimelineState::new(true)))
    }

    pub(in crate::player::backend::ffmpeg::audio) fn with_timeline(
        control: Arc<FfmpegControl>,
        timeline: Arc<AudioTimelineState>,
    ) -> Self {
        Self {
            state: Mutex::new(AudioQueueState::new()),
            ready: Condvar::new(),
            timeline,
            shutdown: AtomicBool::new(false),
            control,
        }
    }

    pub(in crate::player::backend::ffmpeg::audio) fn generation(&self) -> u64 {
        self.timeline.epoch()
    }

    pub(in crate::player::backend::ffmpeg::audio) fn is_current_generation(
        &self,
        generation: u64,
    ) -> bool {
        self.generation() == generation && !self.shutdown.load(Ordering::Acquire)
    }

    pub(in crate::player::backend::ffmpeg::audio) fn snapshot(
        &self,
    ) -> std::result::Result<AudioQueueSnapshot, String> {
        let started_at = Instant::now();
        let lock_started_at = Instant::now();
        let state = self
            .state
            .lock()
            .map_err(|_| "系统音频解码队列已损坏".to_string())?;
        let lock_wait = lock_started_at.elapsed();
        let snapshot = self.snapshot_for_locked_state(&state);
        drop(state);
        log_audio_queue_snapshot_timing(started_at.elapsed(), lock_wait, snapshot);
        Ok(snapshot)
    }

    pub(in crate::player::backend::ffmpeg::audio) fn try_snapshot(
        &self,
    ) -> std::result::Result<Option<AudioQueueSnapshot>, String> {
        let state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Poisoned(_)) => {
                return Err("系统音频解码队列已损坏".to_string());
            }
        };
        Ok(Some(self.snapshot_for_locked_state(&state)))
    }

    fn snapshot_for_locked_state(&self, state: &AudioQueueState) -> AudioQueueSnapshot {
        AudioQueueSnapshot {
            pending_nsecs: state.queued_duration_nsecs,
            queued_nsecs: state.queued_duration_nsecs.saturating_sub(
                state
                    .in_flight
                    .map(|in_flight| in_flight.remaining_duration_nsecs)
                    .unwrap_or_default(),
            ),
            in_flight_nsecs: state
                .in_flight
                .map(|in_flight| in_flight.remaining_duration_nsecs)
                .unwrap_or_default(),
            frames: state.items.len(),
            in_flight_frames: usize::from(state.in_flight.is_some()),
            generation: self.generation(),
            payload_range_nsecs: queue_payload_range(state),
        }
    }

    pub(in crate::player::backend::ffmpeg::audio) fn clear_current_epoch(&self) {
        if let Ok(mut state) = self.state.lock() {
            let stale_items = state
                .items
                .len()
                .saturating_add(usize::from(state.in_flight.is_some()));
            self.timeline
                .record_stale_queue_items(u64::try_from(stale_items).unwrap_or(u64::MAX));
            state.clear();
        }
        self.ready.notify_all();
    }

    pub(in crate::player::backend::ffmpeg::audio) fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.ready.notify_all();
    }

    pub(in crate::player::backend::ffmpeg::audio) fn pop(
        &self,
    ) -> std::result::Result<Option<AudioQueueItem>, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "系统音频解码队列已损坏".to_string())?;
        while state.items.is_empty() || !self.timeline.active() {
            if self.shutdown.load(Ordering::Acquire) || self.control.should_stop() {
                return Ok(None);
            }
            state = self
                .ready
                .wait(state)
                .map_err(|_| "系统音频解码队列已损坏".to_string())?;
        }
        if self.shutdown.load(Ordering::Acquire) || self.control.should_stop() {
            Ok(None)
        } else {
            let mutation = self.timeline.begin_mutation();
            let item = state.items.pop_front();
            if let Some(item) = item.as_ref() {
                state.in_flight = Some(AudioQueueInFlight {
                    generation: item.generation,
                    start_timeline_nsecs: item.start_timeline_nsecs,
                    end_timeline_nsecs: item.end_timeline_nsecs,
                    remaining_samples: item.samples.len(),
                    remaining_duration_nsecs: item.duration_nsecs,
                });
            }
            drop(mutation);
            Ok(item)
        }
    }

    pub(in crate::player::backend::ffmpeg::audio) fn finish_item(
        &self,
        generation: u64,
        samples: usize,
        duration_nsecs: u64,
    ) {
        self.finish_item_with_lock_checkpoint(generation, samples, duration_nsecs, || {});
    }

    fn finish_item_with_lock_checkpoint(
        &self,
        generation: u64,
        samples: usize,
        duration_nsecs: u64,
        after_queue_lock: impl FnOnce(),
    ) {
        // Take the queue lock before validating the epoch. Reset publishes the
        // new epoch before taking this same lock; this ordering prevents a
        // stale worker that passed a lock-free check from subtracting its old
        // completion from counters already rebuilt for the new generation.
        if let Ok(mut state) = self.state.lock() {
            after_queue_lock();
            if self.generation() == generation {
                let _mutation = self.timeline.begin_mutation();
                state.finish_item(samples, duration_nsecs);
                if let Some(in_flight) = state.in_flight.as_mut()
                    && in_flight.generation == generation
                {
                    in_flight.remaining_samples =
                        in_flight.remaining_samples.saturating_sub(samples);
                    in_flight.remaining_duration_nsecs = in_flight
                        .remaining_duration_nsecs
                        .saturating_sub(duration_nsecs);
                    if in_flight.remaining_samples == 0 || in_flight.remaining_duration_nsecs == 0 {
                        state.in_flight = None;
                    }
                }
            }
        }
        self.ready.notify_all();
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::audio) fn finish_item_with_lock_checkpoint_for_test(
        &self,
        generation: u64,
        samples: usize,
        duration_nsecs: u64,
        after_queue_lock: impl FnOnce(),
    ) {
        self.finish_item_with_lock_checkpoint(
            generation,
            samples,
            duration_nsecs,
            after_queue_lock,
        );
    }
}

fn queue_payload_range(state: &AudioQueueState) -> Option<(u64, u64)> {
    let queued = state
        .items
        .front()
        .zip(state.items.back())
        .map(|(first, last)| (first.start_timeline_nsecs, last.end_timeline_nsecs));
    let in_flight = state.in_flight.map(|in_flight| {
        let consumed_nsecs = in_flight
            .end_timeline_nsecs
            .saturating_sub(in_flight.start_timeline_nsecs)
            .saturating_sub(in_flight.remaining_duration_nsecs);
        (
            in_flight
                .start_timeline_nsecs
                .saturating_add(consumed_nsecs),
            in_flight.end_timeline_nsecs,
        )
    });
    match (queued, in_flight) {
        (Some((queued_start, queued_end)), Some((in_flight_start, in_flight_end))) => Some((
            queued_start.min(in_flight_start),
            queued_end.max(in_flight_end),
        )),
        (Some(range), None) | (None, Some(range)) => Some(range),
        (None, None) => None,
    }
}

pub(in crate::player::backend::ffmpeg::audio) fn spawn_audio_queue_worker(
    shared: Arc<AudioShared>,
    queue: Arc<AudioQueueShared>,
) -> std::result::Result<JoinHandle<()>, String> {
    thread::Builder::new()
        .name("tiny-ffmpeg-audio-output".to_string())
        .spawn(move || run_audio_queue_worker(shared, queue))
        .map_err(|error| format!("启动系统音频输出队列失败：{error}"))
}

fn run_audio_queue_worker(shared: Arc<AudioShared>, queue: Arc<AudioQueueShared>) {
    loop {
        let item = match queue.pop() {
            Ok(Some(item)) => item,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "FFmpeg audio queue worker failed to read decoded audio");
                break;
            }
        };
        let generation = item.generation;
        let samples = item.samples.len();
        let duration_nsecs = item.duration_nsecs;
        let progress = match write_audio_queue_item(&shared, &queue, item) {
            Ok(progress) => progress,
            Err(error) => {
                let progress = error.progress;
                tracing::warn!(%error, "FFmpeg audio queue worker failed to write decoded audio");
                progress
            }
        };
        let remaining_samples = samples.saturating_sub(progress.samples);
        let remaining_duration_nsecs = duration_nsecs.saturating_sub(progress.duration_nsecs);
        if remaining_samples > 0 || remaining_duration_nsecs > 0 {
            if !queue.is_current_generation(generation) {
                queue.timeline.record_stale_queue_item();
            }
            queue.finish_item(generation, remaining_samples, remaining_duration_nsecs);
        }
    }
}

pub(in crate::player::backend::ffmpeg::audio) fn write_audio_queue_item(
    shared: &AudioShared,
    queue: &AudioQueueShared,
    item: AudioQueueItem,
) -> std::result::Result<AudioQueueWriteProgress, AudioQueueWriteError> {
    let mut offset = 0;
    let total_samples = item.samples.len();
    let mut progress = AudioQueueWriteProgress::default();
    let mut wait_started_at = None;
    let mut next_wait_log_at = None;

    while offset < item.samples.len() {
        if shared.control.should_interrupt()
            || !queue.timeline.active()
            || !queue.is_current_generation(item.generation)
            || !shared.timeline.is_current_epoch(item.generation)
        {
            queue.timeline.record_stale_queue_item();
            return Ok(progress);
        }

        let mut guard = shared
            .buffer
            .lock()
            .map_err(|_| AudioQueueWriteError::new("系统音频缓冲区已损坏", progress))?;
        while guard.available_capacity() == 0
            && !shared.control.should_interrupt()
            && queue.timeline.active()
            && queue.is_current_generation(item.generation)
            && shared.timeline.is_current_epoch(item.generation)
        {
            let (next_guard, _) = shared
                .ready
                .wait_timeout(guard, SCHEDULER_POLL_INTERVAL)
                .map_err(|_| AudioQueueWriteError::new("系统音频缓冲区已损坏", progress))?;
            guard = next_guard;

            let now = Instant::now();
            let wait_started = *wait_started_at.get_or_insert(now);
            if next_wait_log_at.is_none() {
                next_wait_log_at = now.checked_add(AUDIO_QUEUE_WAIT_LOG_AFTER);
            } else if next_wait_log_at.is_some_and(|deadline| now >= deadline) {
                tracing::debug!(
                    waited_ms = now.saturating_duration_since(wait_started).as_secs_f64() * 1000.0,
                    queued_samples = guard.len(),
                    total_samples,
                    written_samples = offset,
                    "waiting for native audio output ring buffer space"
                );
                next_wait_log_at = now.checked_add(AUDIO_QUEUE_WAIT_LOG_AFTER);
            }
        }

        if shared.control.should_interrupt()
            || !queue.timeline.active()
            || !queue.is_current_generation(item.generation)
            || !shared.timeline.is_current_epoch(item.generation)
        {
            queue.timeline.record_stale_queue_item();
            return Ok(progress);
        }

        let capacity = guard.available_capacity();
        if capacity == 0 {
            continue;
        }
        if guard.epoch != item.generation {
            queue.timeline.record_stale_queue_item();
            return Ok(progress);
        }
        let previous_offset = offset;
        let end = (offset + capacity).min(item.samples.len());
        let mutation = queue.timeline.begin_mutation();
        let written = guard.push_slice(&item.samples[offset..end]);
        offset += written;

        if total_samples > 0 && written > 0 {
            let previous_timeline_nsecs = interpolated_audio_timeline_nsecs(
                item.start_timeline_nsecs,
                item.end_timeline_nsecs,
                previous_offset,
                total_samples,
            );
            let current_timeline_nsecs = interpolated_audio_timeline_nsecs(
                item.start_timeline_nsecs,
                item.end_timeline_nsecs,
                offset,
                total_samples,
            );
            // The buffer lock serializes this timeline publication with reset.
            // A stale worker can therefore never overwrite the new epoch's end.
            shared.set_queued_end_timeline_nsecs(current_timeline_nsecs);
            let written_duration_nsecs =
                current_timeline_nsecs.saturating_sub(previous_timeline_nsecs);
            drop(guard);
            queue.finish_item(item.generation, written, written_duration_nsecs);
            progress.samples = progress.samples.saturating_add(written);
            progress.duration_nsecs = progress
                .duration_nsecs
                .saturating_add(written_duration_nsecs);
        } else {
            drop(guard);
        }
        drop(mutation);
        shared.ready.notify_all();
    }

    Ok(progress)
}
