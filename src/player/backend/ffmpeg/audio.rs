use super::*;

pub(super) struct AudioOutput {
    shared: Arc<AudioShared>,
    queue: Arc<AudioQueueShared>,
    queue_worker: Option<JoinHandle<()>>,
    _stream: cpal::Stream,
    sample_rate: c_int,
    channels: c_int,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AudioOutputDrainStatus {
    Drained,
    Waiting,
    Interrupted,
}

pub(super) enum AudioOutputPushResult {
    Queued,
    WouldBlock {
        samples: Vec<f32>,
        queued_frames: usize,
        queued_duration: Duration,
    },
    Interrupted {
        samples: Vec<f32>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AudioClockMode {
    SyncingVideo,
    AudioStarted,
    UnderrunRecovery,
}

impl AudioClockMode {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::SyncingVideo => "syncing_video",
            Self::AudioStarted => "audio_started",
            Self::UnderrunRecovery => "underrun_recovery",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AudioOutputSnapshot {
    pub(super) played_timeline_nsecs: u64,
    pub(super) buffered_until_timeline_nsecs: u64,
    pub(super) shared_pending_nsecs: u64,
    pub(super) queue_pending_nsecs: u64,
    pub(super) total_pending_nsecs: u64,
    pub(super) queue_frames: usize,
    pub(super) queue_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AudioQueueSnapshot {
    pending_nsecs: u64,
    frames: usize,
    generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AudioSharedSnapshot {
    played_timeline_nsecs: u64,
    pending_nsecs: u64,
}

pub(super) struct AudioShared {
    pub(super) buffer: Mutex<AudioBuffer>,
    pub(super) ready: Condvar,
    pub(super) played_samples: AtomicU64,
    pub(super) queued_end_timeline_nsecs: AtomicU64,
    output_delay_nsecs: AtomicU64,
    output_delay_updated_nsecs: AtomicU64,
    callback_count: AtomicU64,
    underrun_count: AtomicU64,
    underrun_active: AtomicBool,
    underrun_timeline_nsecs: AtomicU64,
    misaligned_audio_buffer_count: AtomicU64,
    last_callback_nsecs: AtomicU64,
    clock_start: Instant,
    sample_rate: c_int,
    channels: c_int,
    pub(super) control: Arc<FfmpegControl>,
}

pub(super) struct AudioBuffer {
    samples: Vec<f32>,
    read_pos: usize,
    write_pos: usize,
    len: usize,
}

struct AudioQueueItem {
    samples: Vec<f32>,
    start_timeline_nsecs: u64,
    end_timeline_nsecs: u64,
    duration_nsecs: u64,
    generation: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AudioQueueWriteProgress {
    samples: usize,
    duration_nsecs: u64,
}

#[derive(Debug)]
struct AudioQueueWriteError {
    message: String,
    progress: AudioQueueWriteProgress,
}

impl AudioQueueWriteError {
    fn new(message: impl Into<String>, progress: AudioQueueWriteProgress) -> Self {
        Self {
            message: message.into(),
            progress,
        }
    }
}

impl std::fmt::Display for AudioQueueWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub(super) struct AudioQueueShared {
    state: Mutex<AudioQueueState>,
    ready: Condvar,
    generation: AtomicU64,
    shutdown: AtomicBool,
    control: Arc<FfmpegControl>,
}

pub(super) struct AudioQueueState {
    items: VecDeque<AudioQueueItem>,
    queued_samples: usize,
    queued_duration_nsecs: u64,
}

impl AudioBuffer {
    pub(super) fn with_capacity(max_samples: usize) -> Self {
        Self {
            samples: vec![0.0; max_samples],
            read_pos: 0,
            write_pos: 0,
            len: 0,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(super) fn available_capacity(&self) -> usize {
        self.samples.len().saturating_sub(self.len)
    }

    pub(super) fn clear(&mut self) {
        self.read_pos = 0;
        self.write_pos = 0;
        self.len = 0;
    }

    pub(super) fn push_slice(&mut self, samples: &[f32]) -> usize {
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

    pub(super) fn pop_sample(&mut self) -> Option<f32> {
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
    fn new() -> Self {
        Self {
            items: VecDeque::new(),
            queued_samples: 0,
            queued_duration_nsecs: 0,
        }
    }

    fn can_accept(&self) -> bool {
        self.queued_duration_nsecs == 0
            || self.queued_duration_nsecs < duration_nsecs(AUDIO_OUTPUT_QUEUE_LIMIT_DURATION)
    }

    fn push(&mut self, item: AudioQueueItem) {
        self.queued_samples = self.queued_samples.saturating_add(item.samples.len());
        self.queued_duration_nsecs = self
            .queued_duration_nsecs
            .saturating_add(item.duration_nsecs);
        self.items.push_back(item);
    }

    fn finish_item(&mut self, samples: usize, duration_nsecs: u64) {
        self.queued_samples = self.queued_samples.saturating_sub(samples);
        self.queued_duration_nsecs = self.queued_duration_nsecs.saturating_sub(duration_nsecs);
    }

    fn clear(&mut self) {
        self.items.clear();
        self.queued_samples = 0;
        self.queued_duration_nsecs = 0;
    }

    fn pending_duration(&self) -> Duration {
        Duration::from_nanos(self.queued_duration_nsecs)
    }
}

impl AudioQueueShared {
    fn new(control: Arc<FfmpegControl>) -> Self {
        Self {
            state: Mutex::new(AudioQueueState::new()),
            ready: Condvar::new(),
            generation: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            control,
        }
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn is_current_generation(&self, generation: u64) -> bool {
        self.generation() == generation && !self.shutdown.load(Ordering::Acquire)
    }

    fn snapshot(&self) -> std::result::Result<AudioQueueSnapshot, String> {
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

    fn clear(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut state) = self.state.lock() {
            state.clear();
        }
        self.ready.notify_all();
    }

    fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.ready.notify_all();
    }

    fn pop(&self) -> std::result::Result<Option<AudioQueueItem>, String> {
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

    fn finish_item(&self, generation: u64, samples: usize, duration_nsecs: u64) {
        if self.generation() == generation
            && let Ok(mut state) = self.state.lock()
        {
            state.finish_item(samples, duration_nsecs);
        }
        self.ready.notify_all();
    }
}

impl AudioShared {
    pub(super) fn new(
        max_samples: usize,
        sample_rate: c_int,
        channels: c_int,
        control: Arc<FfmpegControl>,
    ) -> Self {
        Self {
            buffer: Mutex::new(AudioBuffer::with_capacity(max_samples)),
            ready: Condvar::new(),
            played_samples: AtomicU64::new(0),
            queued_end_timeline_nsecs: AtomicU64::new(0),
            output_delay_nsecs: AtomicU64::new(0),
            output_delay_updated_nsecs: AtomicU64::new(0),
            callback_count: AtomicU64::new(0),
            underrun_count: AtomicU64::new(0),
            underrun_active: AtomicBool::new(false),
            underrun_timeline_nsecs: AtomicU64::new(0),
            misaligned_audio_buffer_count: AtomicU64::new(0),
            last_callback_nsecs: AtomicU64::new(0),
            clock_start: Instant::now(),
            sample_rate,
            channels,
            control,
        }
    }

    pub(super) fn reset_clock(&self, timeline_nsecs: u64) {
        let started_at = Instant::now();
        let lock_started_at = Instant::now();
        let lock_result = self.buffer.lock();
        let lock_wait = lock_started_at.elapsed();
        let buffer_cleared = lock_result.is_ok();
        if let Ok(mut guard) = lock_result {
            guard.clear();
            self.ready.notify_all();
        }
        self.played_samples.store(
            audio_elements_for_duration_floor(timeline_nsecs, self.sample_rate, self.channels),
            Ordering::Relaxed,
        );
        self.queued_end_timeline_nsecs
            .store(timeline_nsecs, Ordering::Relaxed);
        self.update_output_delay(Duration::ZERO);
        self.clear_underrun();
        log_audio_shared_reset_clock_timing(
            timeline_nsecs,
            started_at.elapsed(),
            lock_wait,
            buffer_cleared,
        );
    }

    pub(super) fn set_queued_end_timeline_nsecs(&self, timeline_nsecs: u64) {
        self.queued_end_timeline_nsecs
            .store(timeline_nsecs, Ordering::Relaxed);
    }

    #[cfg(test)]
    fn queued_duration(&self) -> std::result::Result<Duration, String> {
        let queued_samples = self
            .buffer
            .lock()
            .map_err(|_| "系统音频缓冲区已损坏".to_string())?
            .len();
        Ok(audio_elements_duration(
            queued_samples,
            self.sample_rate,
            self.channels,
        ))
    }

    #[cfg(test)]
    fn queued_duration_nsecs(&self) -> u64 {
        self.queued_duration()
            .map(duration_nsecs)
            .unwrap_or_default()
    }

    fn output_delay_nsecs(&self) -> u64 {
        let delay = self.output_delay_nsecs.load(Ordering::Relaxed);
        if delay == 0 {
            return 0;
        }
        let updated = self.output_delay_updated_nsecs.load(Ordering::Relaxed);
        let elapsed = duration_nsecs(self.clock_start.elapsed()).saturating_sub(updated);
        delay.saturating_sub(elapsed)
    }

    fn update_output_delay(&self, delay: Duration) {
        let delay = delay.min(AUDIO_OUTPUT_DELAY_LIMIT);
        self.output_delay_nsecs
            .store(duration_nsecs(delay), Ordering::Relaxed);
        self.output_delay_updated_nsecs.store(
            duration_nsecs(self.clock_start.elapsed()),
            Ordering::Relaxed,
        );
    }

    fn played_timeline_nsecs_for_pending(&self, pending_nsecs: u64) -> u64 {
        if self.underrun_active.load(Ordering::Acquire) {
            return self.underrun_timeline_nsecs.load(Ordering::Acquire);
        }
        self.played_timeline_nsecs_from_pending(pending_nsecs)
    }

    fn played_timeline_nsecs_from_pending(&self, pending_nsecs: u64) -> u64 {
        self.queued_end_timeline_nsecs
            .load(Ordering::Relaxed)
            .saturating_sub(pending_nsecs)
            .saturating_sub(self.output_delay_nsecs())
    }

    fn mark_underrun(&self, played_timeline_nsecs: u64) -> bool {
        match self.underrun_active.compare_exchange(
            false,
            true,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                self.underrun_timeline_nsecs
                    .store(played_timeline_nsecs, Ordering::Release);
                true
            }
            Err(_) => false,
        }
    }

    fn clear_underrun(&self) {
        self.underrun_active.store(false, Ordering::Release);
    }

    fn clear_underrun_if_recovered(&self, pending_nsecs: u64) {
        if pending_nsecs >= duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION) {
            self.clear_underrun();
        }
    }

    #[cfg(test)]
    pub(super) fn underrun_active_for_test(&self) -> bool {
        self.underrun_active.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn clear_underrun_if_recovered_for_test(&self, pending_nsecs: u64) {
        self.clear_underrun_if_recovered(pending_nsecs);
    }

    #[cfg(test)]
    pub(super) fn played_timeline_nsecs(&self) -> u64 {
        self.played_timeline_nsecs_for_pending(self.queued_duration_nsecs())
    }

    fn snapshot(&self) -> std::result::Result<AudioSharedSnapshot, String> {
        let started_at = Instant::now();
        let lock_started_at = Instant::now();
        let queued_samples = self
            .buffer
            .lock()
            .map_err(|_| "系统音频缓冲区已损坏".to_string())?
            .len();
        let buffer_lock_wait = lock_started_at.elapsed();
        let queued_duration_nsecs = duration_nsecs(audio_elements_duration(
            queued_samples,
            self.sample_rate,
            self.channels,
        ));
        let output_delay_nsecs = self.output_delay_nsecs();
        let pending_nsecs = queued_duration_nsecs.saturating_add(output_delay_nsecs);
        let played_timeline_nsecs = self.played_timeline_nsecs_for_pending(queued_duration_nsecs);
        let snapshot = AudioSharedSnapshot {
            played_timeline_nsecs,
            pending_nsecs,
        };
        log_audio_shared_snapshot_timing(
            started_at.elapsed(),
            buffer_lock_wait,
            queued_samples,
            snapshot,
        );
        Ok(snapshot)
    }

    #[cfg(test)]
    pub(super) fn set_output_delay_for_test(&self, delay: Duration) {
        self.output_delay_nsecs
            .store(duration_nsecs(delay), Ordering::Relaxed);
        self.output_delay_updated_nsecs.store(
            duration_nsecs(self.clock_start.elapsed()).saturating_add(1_000_000_000),
            Ordering::Relaxed,
        );
    }
}

impl AudioOutput {
    pub(super) fn new(control: Arc<FfmpegControl>) -> std::result::Result<Self, String> {
        let host = cpal::default_host();
        let mut last_error = None;
        for candidate in output_device_candidates(&host)? {
            match Self::from_device(
                candidate.device,
                candidate.name.clone(),
                Arc::clone(&control),
            ) {
                Ok(output) => return Ok(output),
                Err(error) => {
                    tracing::warn!(
                        device = %candidate.name,
                        source = %candidate.source,
                        %error,
                        "native audio output device initialization failed"
                    );
                    last_error = Some(error);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| "未找到系统音频输出设备".to_string()))
    }

    fn from_device(
        device: cpal::Device,
        device_name: String,
        control: Arc<FfmpegControl>,
    ) -> std::result::Result<Self, String> {
        let supported_config = device
            .default_output_config()
            .map_err(|error| format!("读取系统音频输出配置失败：{error}"))?;
        let sample_rate = c_int::try_from(supported_config.sample_rate())
            .map_err(|_| "系统音频采样率过大".to_string())?;
        let channels = c_int::from(supported_config.channels());
        let max_samples = usize::try_from(sample_rate)
            .ok()
            .and_then(|rate| rate.checked_mul(usize::try_from(channels).ok()?))
            .and_then(|samples| samples.checked_mul(AUDIO_BUFFER_SECONDS))
            .ok_or_else(|| "系统音频缓冲区过大".to_string())?;
        let shared = Arc::new(AudioShared::new(
            max_samples,
            sample_rate,
            channels,
            Arc::clone(&control),
        ));
        let config: cpal::StreamConfig = supported_config.clone().into();
        let sample_format = supported_config.sample_format();
        tracing::debug!(
            device = %device_name,
            sample_rate,
            channels,
            ?sample_format,
            "selected native audio output config"
        );
        let stream = match sample_format {
            cpal::SampleFormat::I8 => {
                build_audio_output_stream::<i8>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::I16 => {
                build_audio_output_stream::<i16>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::I32 => {
                build_audio_output_stream::<i32>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::I64 => {
                build_audio_output_stream::<i64>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::U8 => {
                build_audio_output_stream::<u8>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::U16 => {
                build_audio_output_stream::<u16>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::U32 => {
                build_audio_output_stream::<u32>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::U64 => {
                build_audio_output_stream::<u64>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::F32 => {
                build_audio_output_stream::<f32>(&device, &config, shared.clone())
            }
            cpal::SampleFormat::F64 => {
                build_audio_output_stream::<f64>(&device, &config, shared.clone())
            }
            sample_format => {
                return Err(format!("暂不支持的系统音频采样格式：{sample_format:?}"));
            }
        }
        .map_err(|error| format!("创建系统音频输出流失败：{error}"))?;
        stream
            .play()
            .map_err(|error| format!("启动系统音频输出流失败：{error}"))?;
        let queue = Arc::new(AudioQueueShared::new(Arc::clone(&control)));
        let queue_worker = spawn_audio_queue_worker(Arc::clone(&shared), Arc::clone(&queue))?;

        Ok(Self {
            shared,
            queue,
            queue_worker: Some(queue_worker),
            _stream: stream,
            sample_rate,
            channels,
        })
    }

    pub(super) fn sample_rate(&self) -> c_int {
        self.sample_rate
    }

    pub(super) fn channels(&self) -> c_int {
        self.channels
    }

    pub(super) fn misaligned_audio_buffer_count(&self) -> u64 {
        self.shared
            .misaligned_audio_buffer_count
            .load(Ordering::Relaxed)
    }

    pub(super) fn try_push_timed(
        &self,
        mut samples: Vec<f32>,
        start_timeline_nsecs: u64,
        end_timeline_nsecs: u64,
        control: &FfmpegControl,
    ) -> std::result::Result<AudioOutputPushResult, String> {
        let started_at = Instant::now();
        let original_sample_count = samples.len();
        let aligned_sample_count =
            align_audio_elements_to_frame_boundary(original_sample_count, self.channels);
        if aligned_sample_count < original_sample_count {
            samples.truncate(aligned_sample_count);
            let misaligned_audio_buffer_count = self
                .shared
                .misaligned_audio_buffer_count
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            tracing::warn!(
                original_sample_count,
                aligned_sample_count,
                dropped_audio_elements = original_sample_count.saturating_sub(aligned_sample_count),
                channels = self.channels,
                misaligned_audio_buffer_count,
                "truncated misaligned interleaved audio buffer before native output queue"
            );
        }
        if samples.is_empty() || end_timeline_nsecs <= start_timeline_nsecs {
            log_audio_output_try_push_timed_timing(AudioOutputTryPushTimedTiming {
                result: "queued_empty",
                total: started_at.elapsed(),
                queue_lock_wait: Duration::ZERO,
                sample_count: samples.len(),
                misaligned_audio_buffer_count: self.misaligned_audio_buffer_count(),
                start_timeline_nsecs,
                end_timeline_nsecs,
                queued_frames: 0,
                queued_duration: Duration::ZERO,
            });
            return Ok(AudioOutputPushResult::Queued);
        }

        let generation = self.queue.generation();
        if control.should_interrupt() || !self.queue.is_current_generation(generation) {
            log_audio_output_try_push_timed_timing(AudioOutputTryPushTimedTiming {
                result: "interrupted",
                total: started_at.elapsed(),
                queue_lock_wait: Duration::ZERO,
                sample_count: samples.len(),
                misaligned_audio_buffer_count: self.misaligned_audio_buffer_count(),
                start_timeline_nsecs,
                end_timeline_nsecs,
                queued_frames: 0,
                queued_duration: Duration::ZERO,
            });
            return Ok(AudioOutputPushResult::Interrupted { samples });
        }

        let duration_nsecs = end_timeline_nsecs.saturating_sub(start_timeline_nsecs);
        let sample_count = samples.len();
        let lock_started_at = Instant::now();
        let mut state = self
            .queue
            .state
            .lock()
            .map_err(|_| "系统音频解码队列已损坏".to_string())?;
        let queue_lock_wait = lock_started_at.elapsed();
        if state.can_accept() {
            state.push(AudioQueueItem {
                samples,
                start_timeline_nsecs,
                end_timeline_nsecs,
                duration_nsecs,
                generation,
            });
            let queued_frames = state.items.len();
            let queued_duration = state.pending_duration();
            drop(state);
            self.queue.ready.notify_all();
            log_audio_output_try_push_timed_timing(AudioOutputTryPushTimedTiming {
                result: "queued",
                total: started_at.elapsed(),
                queue_lock_wait,
                sample_count,
                misaligned_audio_buffer_count: self.misaligned_audio_buffer_count(),
                start_timeline_nsecs,
                end_timeline_nsecs,
                queued_frames,
                queued_duration,
            });
            return Ok(AudioOutputPushResult::Queued);
        }

        let queued_frames = state.items.len();
        let queued_duration = state.pending_duration();
        drop(state);
        log_audio_output_try_push_timed_timing(AudioOutputTryPushTimedTiming {
            result: "would_block",
            total: started_at.elapsed(),
            queue_lock_wait,
            sample_count,
            misaligned_audio_buffer_count: self.misaligned_audio_buffer_count(),
            start_timeline_nsecs,
            end_timeline_nsecs,
            queued_frames,
            queued_duration,
        });
        Ok(AudioOutputPushResult::WouldBlock {
            samples,
            queued_frames,
            queued_duration,
        })
    }

    pub(super) fn reset_clock(&self, timeline_nsecs: u64) {
        let started_at = Instant::now();
        let queue_started_at = Instant::now();
        self.queue.clear();
        let queue_clear = queue_started_at.elapsed();
        let shared_started_at = Instant::now();
        self.shared.reset_clock(timeline_nsecs);
        let shared_reset = shared_started_at.elapsed();
        log_audio_output_reset_clock_timing(
            timeline_nsecs,
            started_at.elapsed(),
            queue_clear,
            shared_reset,
        );
    }

    pub(super) fn underrun_active(&self) -> bool {
        self.shared.underrun_active.load(Ordering::Acquire)
    }

    pub(super) fn drain_deadline(&self) -> std::result::Result<Option<Instant>, String> {
        let timeout = Duration::from_nanos(self.snapshot()?.total_pending_nsecs)
            .saturating_add(Duration::from_millis(250));
        Ok(Instant::now().checked_add(timeout))
    }

    pub(super) fn drain_step(
        &self,
        deadline: Instant,
        control: &FfmpegControl,
    ) -> std::result::Result<AudioOutputDrainStatus, String> {
        if control.should_interrupt() {
            return Ok(AudioOutputDrainStatus::Interrupted);
        }
        let snapshot = self.snapshot()?;
        if snapshot.total_pending_nsecs == 0 {
            return Ok(AudioOutputDrainStatus::Drained);
        }
        if Instant::now() < deadline {
            return Ok(AudioOutputDrainStatus::Waiting);
        }
        let remaining_samples = self
            .shared
            .buffer
            .lock()
            .map_err(|_| "系统音频缓冲区已损坏".to_string())?
            .len();
        tracing::debug!(
            remaining_samples,
            queued_audio_ms = snapshot.queue_pending_nsecs as f64 / 1_000_000.0,
            "timed out waiting for native audio output to drain"
        );
        Ok(AudioOutputDrainStatus::Drained)
    }

    pub(super) fn snapshot(&self) -> std::result::Result<AudioOutputSnapshot, String> {
        let started_at = Instant::now();
        let shared_started_at = Instant::now();
        let mut shared = self.shared.snapshot()?;
        let mut shared_snapshot = shared_started_at.elapsed();
        let queue_started_at = Instant::now();
        let queue = self.queue.snapshot()?;
        let queue_snapshot = queue_started_at.elapsed();
        let total_pending_nsecs = shared.pending_nsecs.saturating_add(queue.pending_nsecs);
        let mut underrun_recheck = Duration::ZERO;
        if self.shared.underrun_active.load(Ordering::Acquire) {
            self.shared.clear_underrun_if_recovered(total_pending_nsecs);
            if !self.shared.underrun_active.load(Ordering::Acquire) {
                let recheck_started_at = Instant::now();
                shared = self.shared.snapshot()?;
                underrun_recheck = recheck_started_at.elapsed();
                shared_snapshot += underrun_recheck;
            }
        }
        let snapshot = AudioOutputSnapshot {
            played_timeline_nsecs: shared.played_timeline_nsecs,
            buffered_until_timeline_nsecs: shared
                .played_timeline_nsecs
                .saturating_add(total_pending_nsecs),
            shared_pending_nsecs: shared.pending_nsecs,
            queue_pending_nsecs: queue.pending_nsecs,
            total_pending_nsecs,
            queue_frames: queue.frames,
            queue_generation: queue.generation,
        };
        log_audio_output_snapshot_timing(AudioOutputSnapshotTiming {
            total: started_at.elapsed(),
            shared_snapshot,
            queue_snapshot,
            underrun_recheck,
            misaligned_audio_buffer_count: self.misaligned_audio_buffer_count(),
            snapshot,
        });
        Ok(snapshot)
    }
}

struct AudioOutputTryPushTimedTiming {
    result: &'static str,
    total: Duration,
    queue_lock_wait: Duration,
    sample_count: usize,
    misaligned_audio_buffer_count: u64,
    start_timeline_nsecs: u64,
    end_timeline_nsecs: u64,
    queued_frames: usize,
    queued_duration: Duration,
}

struct AudioOutputSnapshotTiming {
    total: Duration,
    shared_snapshot: Duration,
    queue_snapshot: Duration,
    underrun_recheck: Duration,
    misaligned_audio_buffer_count: u64,
    snapshot: AudioOutputSnapshot,
}

fn log_audio_output_try_push_timed_timing(timing: AudioOutputTryPushTimedTiming) {
    tracing::trace!(
        result = timing.result,
        total_ms = timing.total.as_secs_f64() * 1000.0,
        queue_lock_wait_ms = timing.queue_lock_wait.as_secs_f64() * 1000.0,
        sample_count = timing.sample_count,
        misaligned_audio_buffer_count = timing.misaligned_audio_buffer_count,
        start_timeline_nsecs = timing.start_timeline_nsecs,
        end_timeline_nsecs = timing.end_timeline_nsecs,
        queued_frames = timing.queued_frames,
        queued_ms = timing.queued_duration.as_secs_f64() * 1000.0,
        "native audio output try_push_timed timing"
    );
    if timing.total < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
        && timing.queue_lock_wait < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        result = timing.result,
        total_ms = timing.total.as_secs_f64() * 1000.0,
        queue_lock_wait_ms = timing.queue_lock_wait.as_secs_f64() * 1000.0,
        sample_count = timing.sample_count,
        misaligned_audio_buffer_count = timing.misaligned_audio_buffer_count,
        start_timeline_nsecs = timing.start_timeline_nsecs,
        end_timeline_nsecs = timing.end_timeline_nsecs,
        queued_frames = timing.queued_frames,
        queued_ms = timing.queued_duration.as_secs_f64() * 1000.0,
        "native audio output try_push_timed completed slowly"
    );
}

fn log_audio_output_reset_clock_timing(
    timeline_nsecs: u64,
    total: Duration,
    queue_clear: Duration,
    shared_reset: Duration,
) {
    tracing::trace!(
        timeline_nsecs,
        total_ms = total.as_secs_f64() * 1000.0,
        queue_clear_ms = queue_clear.as_secs_f64() * 1000.0,
        shared_reset_ms = shared_reset.as_secs_f64() * 1000.0,
        "native audio output reset_clock timing"
    );
    if total < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
        && queue_clear < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
        && shared_reset < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        timeline_nsecs,
        total_ms = total.as_secs_f64() * 1000.0,
        queue_clear_ms = queue_clear.as_secs_f64() * 1000.0,
        shared_reset_ms = shared_reset.as_secs_f64() * 1000.0,
        "native audio output reset_clock completed slowly"
    );
}

fn log_audio_output_snapshot_timing(timing: AudioOutputSnapshotTiming) {
    tracing::trace!(
        total_ms = timing.total.as_secs_f64() * 1000.0,
        shared_snapshot_ms = timing.shared_snapshot.as_secs_f64() * 1000.0,
        queue_snapshot_ms = timing.queue_snapshot.as_secs_f64() * 1000.0,
        underrun_recheck_ms = timing.underrun_recheck.as_secs_f64() * 1000.0,
        misaligned_audio_buffer_count = timing.misaligned_audio_buffer_count,
        played_timeline_nsecs = timing.snapshot.played_timeline_nsecs,
        pending_ms = timing.snapshot.total_pending_nsecs as f64 / 1_000_000.0,
        shared_pending_ms = timing.snapshot.shared_pending_nsecs as f64 / 1_000_000.0,
        queue_pending_ms = timing.snapshot.queue_pending_nsecs as f64 / 1_000_000.0,
        queue_frames = timing.snapshot.queue_frames,
        queue_generation = timing.snapshot.queue_generation,
        "native audio output snapshot timing"
    );
    if timing.total < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
        && timing.shared_snapshot < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
        && timing.queue_snapshot < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
        && timing.underrun_recheck < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        total_ms = timing.total.as_secs_f64() * 1000.0,
        shared_snapshot_ms = timing.shared_snapshot.as_secs_f64() * 1000.0,
        queue_snapshot_ms = timing.queue_snapshot.as_secs_f64() * 1000.0,
        underrun_recheck_ms = timing.underrun_recheck.as_secs_f64() * 1000.0,
        misaligned_audio_buffer_count = timing.misaligned_audio_buffer_count,
        played_timeline_nsecs = timing.snapshot.played_timeline_nsecs,
        pending_ms = timing.snapshot.total_pending_nsecs as f64 / 1_000_000.0,
        shared_pending_ms = timing.snapshot.shared_pending_nsecs as f64 / 1_000_000.0,
        queue_pending_ms = timing.snapshot.queue_pending_nsecs as f64 / 1_000_000.0,
        queue_frames = timing.snapshot.queue_frames,
        queue_generation = timing.snapshot.queue_generation,
        "native audio output snapshot completed slowly"
    );
}

fn log_audio_shared_snapshot_timing(
    total: Duration,
    buffer_lock_wait: Duration,
    queued_samples: usize,
    snapshot: AudioSharedSnapshot,
) {
    tracing::trace!(
        total_ms = total.as_secs_f64() * 1000.0,
        buffer_lock_wait_ms = buffer_lock_wait.as_secs_f64() * 1000.0,
        queued_samples,
        played_timeline_nsecs = snapshot.played_timeline_nsecs,
        pending_ms = snapshot.pending_nsecs as f64 / 1_000_000.0,
        "native audio shared snapshot timing"
    );
    if total < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
        && buffer_lock_wait < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        total_ms = total.as_secs_f64() * 1000.0,
        buffer_lock_wait_ms = buffer_lock_wait.as_secs_f64() * 1000.0,
        queued_samples,
        played_timeline_nsecs = snapshot.played_timeline_nsecs,
        pending_ms = snapshot.pending_nsecs as f64 / 1_000_000.0,
        "native audio shared snapshot completed slowly"
    );
}

fn log_audio_queue_snapshot_timing(
    total: Duration,
    lock_wait: Duration,
    snapshot: AudioQueueSnapshot,
) {
    tracing::trace!(
        total_ms = total.as_secs_f64() * 1000.0,
        lock_wait_ms = lock_wait.as_secs_f64() * 1000.0,
        pending_ms = snapshot.pending_nsecs as f64 / 1_000_000.0,
        frames = snapshot.frames,
        generation = snapshot.generation,
        "native audio queue snapshot timing"
    );
    if total < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
        && lock_wait < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        total_ms = total.as_secs_f64() * 1000.0,
        lock_wait_ms = lock_wait.as_secs_f64() * 1000.0,
        pending_ms = snapshot.pending_nsecs as f64 / 1_000_000.0,
        frames = snapshot.frames,
        generation = snapshot.generation,
        "native audio queue snapshot completed slowly"
    );
}

fn log_audio_shared_reset_clock_timing(
    timeline_nsecs: u64,
    total: Duration,
    buffer_lock_wait: Duration,
    buffer_cleared: bool,
) {
    tracing::trace!(
        timeline_nsecs,
        total_ms = total.as_secs_f64() * 1000.0,
        buffer_lock_wait_ms = buffer_lock_wait.as_secs_f64() * 1000.0,
        buffer_cleared,
        "native audio shared reset_clock timing"
    );
    if total < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
        && buffer_lock_wait < AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER
    {
        return;
    }
    tracing::debug!(
        timeline_nsecs,
        total_ms = total.as_secs_f64() * 1000.0,
        buffer_lock_wait_ms = buffer_lock_wait.as_secs_f64() * 1000.0,
        buffer_cleared,
        "native audio shared reset_clock completed slowly"
    );
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.queue.shutdown();
        self.shared.ready.notify_all();
        if let Some(handle) = self.queue_worker.take()
            && handle.join().is_err()
        {
            tracing::debug!("FFmpeg audio queue worker panicked during shutdown");
        }
    }
}

fn spawn_audio_queue_worker(
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

fn write_audio_queue_item(
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

struct AudioDeviceCandidate {
    source: &'static str,
    name: String,
    device: cpal::Device,
}

impl AudioDeviceCandidate {
    fn new(source: &'static str, name: String, device: cpal::Device) -> Self {
        Self {
            source,
            name,
            device,
        }
    }
}

fn output_device_candidates(
    host: &cpal::Host,
) -> std::result::Result<Vec<AudioDeviceCandidate>, String> {
    let mut devices = match host.output_devices() {
        Ok(devices) => devices
            .map(|device| {
                let name = device_name(&device);
                (name, device)
            })
            .collect::<Vec<_>>(),
        Err(error) => {
            tracing::warn!(%error, "failed to enumerate native audio output devices");
            Vec::new()
        }
    };
    tracing::debug!(
        available_output_devices = ?devices.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        "available native audio output devices"
    );

    let mut candidates = Vec::new();
    if let Ok(requested) = env::var("TINY_AUDIO_DEVICE") {
        let requested = requested.trim();
        if !requested.is_empty() {
            let requested_lower = requested.to_lowercase();
            if let Some((name, device)) = take_output_device(&mut devices, |name| {
                name.to_lowercase().contains(&requested_lower)
            }) {
                tracing::debug!(
                    requested_device = requested,
                    selected_device = %name,
                    "selected requested native audio output device"
                );
                candidates.push(AudioDeviceCandidate::new("requested", name, device));
            } else {
                tracing::warn!(
                    requested_device = requested,
                    "requested native audio output device was not found"
                );
            }
        }
    }

    if let Some((name, device)) = take_output_device(&mut devices, preferred_audio_service_device) {
        tracing::debug!(
            selected_device = %name,
            "selected preferred native audio service device"
        );
        candidates.push(AudioDeviceCandidate::new("preferred", name, device));
    }

    if let Some(device) = host.default_output_device() {
        let name = device_name(&device);
        devices.retain(|(device_name, _)| device_name != &name);
        if !candidates.iter().any(|candidate| candidate.name == name) {
            tracing::debug!(
                default_device = %name,
                "selected default native audio output device"
            );
            candidates.push(AudioDeviceCandidate::new("default", name, device));
        }
    }

    let (mut normal_devices, null_devices): (Vec<_>, Vec<_>) = devices
        .into_iter()
        .partition(|(name, _)| !null_audio_device(name));
    candidates.extend(
        normal_devices
            .drain(..)
            .map(|(name, device)| AudioDeviceCandidate::new("enumerated", name, device)),
    );
    candidates.extend(
        null_devices
            .into_iter()
            .map(|(name, device)| AudioDeviceCandidate::new("null-fallback", name, device)),
    );

    if candidates.is_empty() {
        return Err("未找到系统音频输出设备".to_string());
    }
    Ok(candidates)
}

fn take_output_device<P>(
    devices: &mut Vec<(String, cpal::Device)>,
    predicate: P,
) -> Option<(String, cpal::Device)>
where
    P: Fn(&str) -> bool,
{
    let index = devices.iter().position(|(name, _)| predicate(name))?;
    Some(devices.remove(index))
}

fn preferred_audio_service_device(name: &str) -> bool {
    let name = name.to_lowercase();
    name.contains("pipewire") || name.contains("pulse")
}

fn null_audio_device(name: &str) -> bool {
    let name = name.to_lowercase();
    name == "null" || name.contains("discard")
}

fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|description| description.name().to_string())
        .unwrap_or_else(|error| format!("<读取设备名称失败：{error}>"))
}

fn build_audio_output_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: Arc<AudioShared>,
) -> std::result::Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let error_callback = |error| tracing::warn!(%error, "native audio output stream error");
    device.build_output_stream(
        config,
        move |data: &mut [T], info| fill_audio_output_with_timing(data, info, &shared),
        error_callback,
        None,
    )
}

#[cfg(test)]
pub(super) fn fill_audio_output<T>(data: &mut [T], shared: &AudioShared)
where
    T: Sample + FromSample<f32>,
{
    fill_audio_output_samples(data, shared, None);
}

pub(super) fn fill_audio_output_with_timing<T>(
    data: &mut [T],
    info: &cpal::OutputCallbackInfo,
    shared: &AudioShared,
) where
    T: Sample + FromSample<f32>,
{
    let timestamp = info.timestamp();
    let playback_delay = timestamp.playback.duration_since(&timestamp.callback);
    fill_audio_output_samples(data, shared, playback_delay);
}

fn fill_audio_output_samples<T>(
    data: &mut [T],
    shared: &AudioShared,
    playback_delay: Option<Duration>,
) where
    T: Sample + FromSample<f32>,
{
    let callback_nsecs = duration_nsecs(shared.clock_start.elapsed());
    let callback_index = shared
        .callback_count
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    let previous_callback_nsecs = shared
        .last_callback_nsecs
        .swap(callback_nsecs, Ordering::Relaxed);
    if previous_callback_nsecs > 0 {
        let callback_gap_nsecs = callback_nsecs.saturating_sub(previous_callback_nsecs);
        if callback_gap_nsecs >= duration_nsecs(AUDIO_CALLBACK_GAP_LOG_AFTER) {
            tracing::debug!(
                callback_index,
                callback_gap_ms = callback_gap_nsecs as f64 / 1_000_000.0,
                output_samples = data.len(),
                "native audio output callback gap exceeded threshold"
            );
        }
    }

    let mut guard = shared.buffer.lock().expect("audio output buffer poisoned");
    if shared.control.should_pause_audio_output() {
        for sample in data.iter_mut() {
            *sample = T::from_sample(0.0);
        }
        tracing::trace!(
            callback_index,
            output_samples = data.len(),
            silence_fill_reason = "paused",
            clock_mode = AudioClockMode::AudioStarted.as_str(),
            misaligned_audio_buffer_count =
                shared.misaligned_audio_buffer_count.load(Ordering::Relaxed),
            "native audio output callback filled silence while paused"
        );
        shared.update_output_delay(Duration::ZERO);
        shared.ready.notify_all();
        return;
    }

    let volume = shared.control.volume();
    let mut played = 0u64;
    let output_samples = data.len();
    let queued_samples_before = guard.len();
    for sample in data {
        let value = match guard.pop_sample() {
            Some(value) => {
                played = played.saturating_add(1);
                value * volume
            }
            None => 0.0,
        }
        .clamp(-1.0, 1.0);
        *sample = T::from_sample(value);
    }
    let queued_samples_after = guard.len();
    drop(guard);

    if played > 0 {
        shared.played_samples.fetch_add(played, Ordering::Relaxed);
        let played_duration = audio_elements_duration(
            usize::try_from(played).unwrap_or(usize::MAX),
            shared.sample_rate,
            shared.channels,
        );
        shared.update_output_delay(
            playback_delay
                .unwrap_or_default()
                .saturating_add(played_duration),
        );
    } else {
        shared.update_output_delay(Duration::ZERO);
    }
    let underrun_samples = output_samples.saturating_sub(usize::try_from(played).unwrap_or(0));
    if underrun_samples > 0 {
        let queued_duration_after_nsecs = duration_nsecs(audio_elements_duration(
            queued_samples_after,
            shared.sample_rate,
            shared.channels,
        ));
        let audio_gap_frames = audio_frames_for_elements(underrun_samples, shared.channels);
        let underrun_timeline_nsecs =
            shared.played_timeline_nsecs_from_pending(queued_duration_after_nsecs);
        let underrun_started = shared.mark_underrun(underrun_timeline_nsecs);
        let underrun_index = shared
            .underrun_count
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if underrun_index == 1 || underrun_index.is_multiple_of(120) {
            tracing::debug!(
                callback_index,
                underrun_count = underrun_index,
                underrun_samples,
                audio_gap_frames,
                played_samples = played,
                output_samples,
                queued_samples_before,
                queued_samples_after,
                underrun_started,
                underrun_timeline_nsecs,
                silence_fill_reason = "underrun",
                clock_mode = AudioClockMode::UnderrunRecovery.as_str(),
                misaligned_audio_buffer_count =
                    shared.misaligned_audio_buffer_count.load(Ordering::Relaxed),
                "native audio output callback filled silence after underrun"
            );
        }
    }
    shared.ready.notify_all();
}

pub(super) fn frame_sample_format(
    frame: *mut ffi::AVFrame,
) -> std::result::Result<ffi::AVSampleFormat, String> {
    let format = unsafe { (*frame).format };
    match format {
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_U8 as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_U8)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S16 as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S16)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S32 as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S32)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_DBL as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_DBL)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_U8P as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_U8P)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S16P as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S16P)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S32P as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S32P)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_DBLP as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_DBLP)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S64 as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S64)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S64P as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S64P)
        }
        _ => Err(format!("FFmpeg 音频帧采样格式无效：{format}")),
    }
}

pub(super) fn audio_sample_len(
    samples: c_int,
    channels: c_int,
) -> std::result::Result<usize, String> {
    audio_elements_for_frames_checked(samples, channels)
}

pub(super) fn audio_elements_for_frames_checked(
    frames: c_int,
    channels: c_int,
) -> std::result::Result<usize, String> {
    if frames < 0 || channels <= 0 {
        return Err("音频帧尺寸无效".to_string());
    }
    usize::try_from(frames)
        .ok()
        .and_then(|frames| frames.checked_mul(usize::try_from(channels).ok()?))
        .ok_or_else(|| "音频帧过大".to_string())
}

#[cfg(test)]
pub(super) fn audio_samples_duration(
    samples: usize,
    sample_rate: c_int,
    channels: c_int,
) -> Duration {
    audio_elements_duration(samples, sample_rate, channels)
}

pub(super) fn audio_elements_duration(
    elements: usize,
    sample_rate: c_int,
    channels: c_int,
) -> Duration {
    duration_for_audio_frames(audio_frames_for_elements(elements, channels), sample_rate)
}

pub(super) fn duration_for_audio_frames(frames: u64, sample_rate: c_int) -> Duration {
    if frames == 0 || sample_rate <= 0 {
        return Duration::ZERO;
    }

    let nanos = (frames as u128).saturating_mul(1_000_000_000) / sample_rate as u128;
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

fn interpolated_audio_timeline_nsecs(
    start_timeline_nsecs: u64,
    end_timeline_nsecs: u64,
    written_samples: usize,
    total_samples: usize,
) -> u64 {
    if written_samples >= total_samples || total_samples == 0 {
        return end_timeline_nsecs;
    }

    let duration = end_timeline_nsecs.saturating_sub(start_timeline_nsecs);
    let written_duration =
        (duration as u128).saturating_mul(written_samples as u128) / total_samples as u128;
    start_timeline_nsecs.saturating_add(u64::try_from(written_duration).unwrap_or(u64::MAX))
}

#[cfg(test)]
pub(super) fn samples_for_duration(
    timeline_nsecs: u64,
    sample_rate: c_int,
    channels: c_int,
) -> u64 {
    audio_elements_for_duration_floor(timeline_nsecs, sample_rate, channels)
}

pub(super) fn audio_elements_for_duration_floor(
    timeline_nsecs: u64,
    sample_rate: c_int,
    channels: c_int,
) -> u64 {
    audio_elements_for_frames(
        audio_frames_for_duration_floor(timeline_nsecs, sample_rate),
        channels,
    )
}

#[cfg(test)]
pub(super) fn audio_elements_for_duration_round(
    timeline_nsecs: u64,
    sample_rate: c_int,
    channels: c_int,
) -> u64 {
    audio_elements_for_frames(
        audio_frames_for_duration_round(timeline_nsecs, sample_rate),
        channels,
    )
}

pub(super) fn audio_frames_for_duration_floor(timeline_nsecs: u64, sample_rate: c_int) -> u64 {
    if timeline_nsecs == 0 || sample_rate <= 0 {
        return 0;
    }

    let frames = (timeline_nsecs as u128).saturating_mul(sample_rate as u128) / 1_000_000_000;
    u64::try_from(frames).unwrap_or(u64::MAX)
}

pub(super) fn audio_frames_for_duration_round(timeline_nsecs: u64, sample_rate: c_int) -> u64 {
    if timeline_nsecs == 0 || sample_rate <= 0 {
        return 0;
    }

    let numerator = (timeline_nsecs as u128).saturating_mul(sample_rate as u128);
    let frames = numerator.saturating_add(500_000_000) / 1_000_000_000;
    u64::try_from(frames).unwrap_or(u64::MAX)
}

pub(super) fn audio_elements_for_frames(frames: u64, channels: c_int) -> u64 {
    if frames == 0 || channels <= 0 {
        return 0;
    }

    frames.saturating_mul(channels as u64)
}

pub(super) fn audio_frames_for_elements(elements: usize, channels: c_int) -> u64 {
    if elements == 0 || channels <= 0 {
        return 0;
    }

    u64::try_from(elements / usize::try_from(channels).unwrap_or(usize::MAX)).unwrap_or(u64::MAX)
}

pub(super) fn align_audio_elements_to_frame_boundary(elements: usize, channels: c_int) -> usize {
    if elements == 0 || channels <= 0 {
        return 0;
    }

    let channels = usize::try_from(channels).unwrap_or(usize::MAX);
    elements.saturating_sub(elements % channels)
}

pub(super) fn zeroed_channel_layout() -> ffi::AVChannelLayout {
    unsafe { std::mem::zeroed() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_output_queue_uses_short_output_backpressure_limit() {
        let mut state = AudioQueueState::new();

        assert!(state.can_accept());

        state.queued_duration_nsecs = duration_nsecs(AUDIO_OUTPUT_QUEUE_LIMIT_DURATION) - 1;
        assert!(state.can_accept());

        state.queued_duration_nsecs = duration_nsecs(AUDIO_OUTPUT_QUEUE_LIMIT_DURATION);
        assert!(!state.can_accept());
    }

    #[test]
    fn audio_output_queue_keeps_eac3_recovery_margin() {
        let mut state = AudioQueueState::new();
        state.queued_duration_nsecs = duration_nsecs(
            AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION.saturating_add(Duration::from_millis(32)),
        );

        assert!(state.can_accept());
    }

    #[test]
    fn audio_queue_write_progress_removes_in_flight_pending_duration() {
        let sample_rate = 48_000;
        let channels = 2;
        let samples = vec![0.25; 8];
        let duration_nsecs = duration_nsecs(audio_elements_duration(
            samples.len(),
            sample_rate,
            channels,
        ));
        let start_timeline_nsecs = 1_000_000_000u64;
        let end_timeline_nsecs = start_timeline_nsecs.saturating_add(duration_nsecs);
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
        let shared = AudioShared::new(samples.len(), sample_rate, channels, Arc::clone(&control));
        let queue = AudioQueueShared::new(control);
        {
            let mut state = queue.state.lock().unwrap();
            state.queued_samples = samples.len();
            state.queued_duration_nsecs = duration_nsecs;
        }

        let progress = write_audio_queue_item(
            &shared,
            &queue,
            AudioQueueItem {
                samples,
                start_timeline_nsecs,
                end_timeline_nsecs,
                duration_nsecs,
                generation: queue.generation(),
            },
        )
        .unwrap();

        assert_eq!(progress.samples, 8);
        assert_eq!(progress.duration_nsecs, duration_nsecs);
        assert_eq!(queue.snapshot().unwrap().pending_nsecs, 0);
    }

    #[test]
    fn delayed_start_silence_uses_aligned_interleaved_elements_for_stereo_gaps() {
        let first_gap_frames = audio_frames_for_duration_round(22_780_000, 44_100);
        let first_gap_elements = audio_elements_for_frames(first_gap_frames, 2);
        let second_gap_frames = audio_frames_for_duration_round(24_780_000, 44_100);
        let second_gap_elements = audio_elements_for_frames(second_gap_frames, 2);

        assert_eq!(first_gap_frames, 1_005);
        assert_eq!(first_gap_elements, 2_010);
        assert_eq!(first_gap_elements % 2, 0);
        assert_eq!(second_gap_frames, 1_093);
        assert_eq!(second_gap_elements, 2_186);
        assert_eq!(second_gap_elements % 2, 0);
    }

    #[test]
    fn audio_element_helpers_keep_interleaved_buffers_frame_aligned() {
        assert_eq!(align_audio_elements_to_frame_boundary(2_009, 2), 2_008);
        assert_eq!(align_audio_elements_to_frame_boundary(2_185, 2), 2_184);
        assert_eq!(
            audio_elements_for_duration_floor(22_780_000, 44_100, 2),
            2_008
        );
        assert_eq!(
            audio_elements_for_duration_round(22_780_000, 44_100, 2),
            2_010
        );
    }
}
