use super::{
    AUDIO_OUTPUT_QUEUE_LIMIT_DURATION, AUDIO_QUEUE_WAIT_LOG_AFTER, Arc, AtomicBool, AtomicU64,
    AudioBuffer, AudioQueueItem, AudioQueueShared, AudioQueueSnapshot, AudioQueueState,
    AudioQueueWriteError, AudioQueueWriteProgress, AudioShared, Condvar, Duration, FfmpegControl,
    Instant, JoinHandle, Mutex, Ordering, SCHEDULER_POLL_INTERVAL, VecDeque, duration_nsecs,
    interpolated_audio_timeline_nsecs, log_audio_queue_snapshot_timing, thread,
};

impl AudioBuffer {
    pub(in crate::player::backend::ffmpeg) fn with_capacity(max_samples: usize) -> Self {
        Self {
            samples: vec![0.0; max_samples],
            read_pos: 0,
            write_pos: 0,
            len: 0,
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
        }
    }

    pub(in crate::player::backend::ffmpeg::audio) fn can_accept(&self) -> bool {
        self.queued_duration_nsecs == 0
            || self.queued_duration_nsecs < duration_nsecs(AUDIO_OUTPUT_QUEUE_LIMIT_DURATION)
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
    }

    pub(in crate::player::backend::ffmpeg::audio) fn pending_duration(&self) -> Duration {
        Duration::from_nanos(self.queued_duration_nsecs)
    }
}

impl AudioQueueShared {
    pub(in crate::player::backend::ffmpeg::audio) fn new(control: Arc<FfmpegControl>) -> Self {
        Self {
            state: Mutex::new(AudioQueueState::new()),
            ready: Condvar::new(),
            generation: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            control,
        }
    }

    pub(in crate::player::backend::ffmpeg::audio) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
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
        let snapshot = AudioQueueSnapshot {
            pending_nsecs: state.queued_duration_nsecs,
            frames: state.items.len(),
            generation: self.generation(),
        };
        drop(state);
        log_audio_queue_snapshot_timing(started_at.elapsed(), lock_wait, snapshot);
        Ok(snapshot)
    }

    pub(in crate::player::backend::ffmpeg::audio) fn clear(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut state) = self.state.lock() {
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
        while state.items.is_empty()
            && !self.shutdown.load(Ordering::Acquire)
            && !self.control.should_stop()
        {
            state = self
                .ready
                .wait(state)
                .map_err(|_| "系统音频解码队列已损坏".to_string())?;
        }
        if self.shutdown.load(Ordering::Acquire) || self.control.should_stop() {
            Ok(None)
        } else {
            Ok(state.items.pop_front())
        }
    }

    pub(in crate::player::backend::ffmpeg::audio) fn finish_item(
        &self,
        generation: u64,
        samples: usize,
        duration_nsecs: u64,
    ) {
        if self.generation() == generation
            && let Ok(mut state) = self.state.lock()
        {
            state.finish_item(samples, duration_nsecs);
        }
        self.ready.notify_all();
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
        if shared.control.should_interrupt() || !queue.is_current_generation(item.generation) {
            return Ok(progress);
        }

        let mut guard = shared
            .buffer
            .lock()
            .map_err(|_| AudioQueueWriteError::new("系统音频缓冲区已损坏", progress))?;
        while guard.available_capacity() == 0
            && !shared.control.should_interrupt()
            && queue.is_current_generation(item.generation)
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

        if shared.control.should_interrupt() || !queue.is_current_generation(item.generation) {
            return Ok(progress);
        }

        let capacity = guard.available_capacity();
        if capacity == 0 {
            continue;
        }
        let previous_offset = offset;
        let end = (offset + capacity).min(item.samples.len());
        let written = guard.push_slice(&item.samples[offset..end]);
        offset += written;
        drop(guard);

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
            shared.set_queued_end_timeline_nsecs(current_timeline_nsecs);
            let written_duration_nsecs =
                current_timeline_nsecs.saturating_sub(previous_timeline_nsecs);
            queue.finish_item(item.generation, written, written_duration_nsecs);
            progress.samples = progress.samples.saturating_add(written);
            progress.duration_nsecs = progress
                .duration_nsecs
                .saturating_add(written_duration_nsecs);
        }
        shared.ready.notify_all();
    }

    Ok(progress)
}
