use super::{
    AUDIO_BUFFER_SECONDS, Arc, AudioOutput, AudioOutputActivitySnapshot, AudioOutputDrainStatus,
    AudioOutputPushError, AudioOutputPushResult, AudioOutputServiceStage,
    AudioOutputServiceStageGuard, AudioOutputServiceTelemetry, AudioOutputSnapshot,
    AudioOutputSnapshotTiming, AudioOutputStableSnapshot, AudioOutputTryPushTimedTiming,
    AudioOutputUnstableSnapshot, AudioQueueItem, AudioQueueShared, AudioQueueSnapshot, AudioShared,
    AudioSharedSnapshot, AudioStagedFrame, AudioTimelineState, DeviceTrait, Duration,
    FfmpegControl, Instant, Ordering, StreamTrait, TryLockError,
    align_audio_elements_to_frame_boundary, audio_elements_for_duration_floor,
    build_audio_output_stream, c_int, log_audio_output_reset_clock_timing,
    log_audio_output_snapshot_timing, log_audio_output_try_push_timed_timing,
    output_device_candidates, spawn_audio_output_service_watchdog, spawn_audio_queue_worker,
};
#[cfg(test)]
use super::{AudioOutputServiceStageSnapshot, fill_audio_output, write_audio_queue_item};

const AUDIO_OUTPUT_STABLE_SNAPSHOT_ATTEMPTS: usize = 8;
const AUDIO_OUTPUT_STABLE_SNAPSHOT_CONTENTION_BUDGET: Duration = Duration::from_millis(2);

impl AudioOutput {
    pub(in crate::player::backend::ffmpeg) fn clock_handle(&self) -> super::AudioClockHandle {
        super::AudioClockHandle::new(Arc::clone(&self.shared), Arc::clone(&self.timeline))
    }

    pub(in crate::player::backend::ffmpeg) fn new(
        control: Arc<FfmpegControl>,
    ) -> std::result::Result<Self, String> {
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
        let timeline = Arc::new(AudioTimelineState::new(false));
        let shared = Arc::new(AudioShared::with_timeline(
            max_samples,
            sample_rate,
            channels,
            Arc::clone(&control),
            Arc::clone(&timeline),
        ));
        let config: cpal::StreamConfig = supported_config.clone().into();
        let sample_format = supported_config.sample_format();
        let sample_format_name = format!("{sample_format:?}").to_ascii_lowercase();
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
        let queue = Arc::new(AudioQueueShared::with_timeline(
            Arc::clone(&control),
            Arc::clone(&timeline),
        ));
        let queue_worker = spawn_audio_queue_worker(Arc::clone(&shared), Arc::clone(&queue))?;
        let service_telemetry = Arc::new(AudioOutputServiceTelemetry::new());
        let service_watchdog =
            match spawn_audio_output_service_watchdog(Arc::clone(&service_telemetry)) {
                Ok(watchdog) => watchdog,
                Err(error) => {
                    queue.shutdown();
                    shared.ready.notify_all();
                    let _ = queue_worker.join();
                    return Err(error);
                }
            };

        Ok(Self {
            shared,
            queue,
            timeline,
            queue_worker: Some(queue_worker),
            service_telemetry,
            service_watchdog: Some(service_watchdog),
            _stream: Some(stream),
            stream_active: std::sync::atomic::AtomicBool::new(false),
            stream_play_count: std::sync::atomic::AtomicU64::new(0),
            stream_pause_count: std::sync::atomic::AtomicU64::new(0),
            sample_rate,
            channels,
            sample_format: sample_format_name,
            device_name,
        })
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn stopped_for_test(
        control: Arc<FfmpegControl>,
        max_samples: usize,
        sample_rate: c_int,
        channels: c_int,
    ) -> Self {
        let timeline = Arc::new(AudioTimelineState::new(false));
        let shared = Arc::new(AudioShared::with_timeline(
            max_samples.max(1),
            sample_rate,
            channels,
            Arc::clone(&control),
            Arc::clone(&timeline),
        ));
        let queue = Arc::new(AudioQueueShared::with_timeline(
            control,
            Arc::clone(&timeline),
        ));
        let service_telemetry = Arc::new(AudioOutputServiceTelemetry::new());
        Self {
            shared,
            queue,
            timeline,
            queue_worker: None,
            service_telemetry,
            service_watchdog: None,
            _stream: None,
            stream_active: std::sync::atomic::AtomicBool::new(false),
            stream_play_count: std::sync::atomic::AtomicU64::new(0),
            stream_pause_count: std::sync::atomic::AtomicU64::new(0),
            sample_rate,
            channels,
            sample_format: "f32".to_string(),
            device_name: "test-stopped-output".to_string(),
        }
    }

    pub(in crate::player::backend::ffmpeg) fn sample_rate(&self) -> c_int {
        self.sample_rate
    }

    pub(in crate::player::backend::ffmpeg) fn channels(&self) -> c_int {
        self.channels
    }

    pub(in crate::player::backend::ffmpeg) fn sample_format(&self) -> &str {
        &self.sample_format
    }

    pub(in crate::player::backend::ffmpeg) fn device_name(&self) -> &str {
        &self.device_name
    }

    pub(in crate::player::backend::ffmpeg) fn misaligned_audio_buffer_count(&self) -> u64 {
        self.shared
            .misaligned_audio_buffer_count
            .load(Ordering::Relaxed)
    }

    pub(in crate::player::backend::ffmpeg) fn try_push_timed(
        &self,
        samples: Vec<f32>,
        start_timeline_nsecs: u64,
        end_timeline_nsecs: u64,
        control: &FfmpegControl,
    ) -> std::result::Result<AudioOutputPushResult, AudioOutputPushError> {
        self.try_push_timed_for_epoch(
            samples,
            start_timeline_nsecs,
            end_timeline_nsecs,
            self.timeline.epoch(),
            control,
        )
    }

    pub(in crate::player::backend::ffmpeg) fn try_push_timed_for_epoch(
        &self,
        mut samples: Vec<f32>,
        start_timeline_nsecs: u64,
        end_timeline_nsecs: u64,
        expected_epoch: u64,
        control: &FfmpegControl,
    ) -> std::result::Result<AudioOutputPushResult, AudioOutputPushError> {
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
        if control.should_interrupt()
            || generation != expected_epoch
            || !self.queue.is_current_generation(generation)
        {
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
        let mut state = match self.queue.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => {
                log_audio_output_try_push_timed_timing(AudioOutputTryPushTimedTiming {
                    result: "would_block_lock_contended",
                    total: started_at.elapsed(),
                    queue_lock_wait: lock_started_at.elapsed(),
                    sample_count,
                    misaligned_audio_buffer_count: self.misaligned_audio_buffer_count(),
                    start_timeline_nsecs,
                    end_timeline_nsecs,
                    queued_frames: 0,
                    queued_duration: Duration::ZERO,
                });
                return Ok(AudioOutputPushResult::WouldBlock {
                    samples,
                    queued_frames: 0,
                    queued_duration: Duration::ZERO,
                });
            }
            Err(TryLockError::Poisoned(_)) => {
                return Err(AudioOutputPushError::new(samples, "系统音频解码队列已损坏"));
            }
        };
        let queue_lock_wait = lock_started_at.elapsed();
        if state.can_accept(duration_nsecs)
            && self.queue.generation() == expected_epoch
            && !control.should_interrupt()
        {
            let _mutation = self.timeline.begin_mutation();
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

    pub(in crate::player::backend::ffmpeg) fn reset_clock(&self, timeline_nsecs: u64) {
        let _stage = self.begin_service_stage(AudioOutputServiceStage::ResetClock);
        let started_at = Instant::now();
        // CPAL control calls deliberately happen before any queue/buffer lock.
        // A seek therefore has one pause followed by one reset/epoch advance.
        if let Err(error) = self.pause_stream() {
            tracing::warn!(%error, "failed to pause native audio stream before clock reset");
        }
        self.timeline.set_active(false);
        let reset_mutation = self.timeline.begin_mutation();
        let epoch = self.timeline.advance_epoch();
        let queue_started_at = Instant::now();
        self.queue.clear_current_epoch();
        let queue_clear = queue_started_at.elapsed();
        let shared_started_at = Instant::now();
        self.shared.reset_clock_at_epoch(timeline_nsecs, epoch);
        let shared_reset = shared_started_at.elapsed();
        drop(reset_mutation);
        log_audio_output_reset_clock_timing(
            timeline_nsecs,
            started_at.elapsed(),
            queue_clear,
            shared_reset,
        );
    }

    /// Immediately makes every callback/queue publication from the old clock
    /// epoch stale without waiting for either audio mutex. The regular reset
    /// path performs physical cleanup later; this fence exists so the playback
    /// coordinator can always terminate an expired output transaction.
    pub(in crate::player::backend::ffmpeg) fn fence_clock_without_wait(
        &self,
        timeline_nsecs: u64,
    ) -> u64 {
        self.timeline.set_active(false);
        let mutation = self.timeline.begin_mutation();
        let epoch = self.timeline.advance_epoch();
        self.shared.set_queued_end_timeline_nsecs(timeline_nsecs);
        drop(mutation);
        self.queue.ready.notify_all();
        self.shared.ready.notify_all();
        epoch
    }

    pub(in crate::player::backend::ffmpeg) fn audio_epoch(&self) -> u64 {
        self.timeline.epoch()
    }

    pub(in crate::player::backend::ffmpeg) fn control_seek_generation(&self) -> u64 {
        self.shared.control.seek_generation()
    }

    pub(in crate::player::backend::ffmpeg) fn deactivate(&self) {
        if let Err(error) = self.pause_stream() {
            tracing::warn!(%error, "failed to pause native audio stream while deactivating AO");
        }
        self.timeline.set_active(false);
        self.queue.ready.notify_all();
        self.shared.ready.notify_all();
    }

    pub(in crate::player::backend::ffmpeg) fn activate_audio_output(
        &self,
        expected_epoch: u64,
        expected_seek_generation: u64,
        control: &FfmpegControl,
    ) -> bool {
        if !self.commit_audio_output_control(expected_epoch, expected_seek_generation, control) {
            return false;
        }
        if let Err(error) = self.play_committed_audio_output() {
            tracing::warn!(%error, "failed to start native audio stream after output commit");
            self.timeline.set_active(false);
            control.set_audio_output_lifecycle(super::AudioOutputLifecycle::Ready);
            return false;
        }
        true
    }

    pub(in crate::player::backend::ffmpeg) fn commit_audio_output_control(
        &self,
        expected_epoch: u64,
        expected_seek_generation: u64,
        control: &FfmpegControl,
    ) -> bool {
        let activated = control
            .compare_and_commit_audio_output_start(expected_seek_generation, || {
                self.timeline.activate_if_epoch(expected_epoch)
            });
        if activated {
            self.queue.ready.notify_all();
            self.shared.ready.notify_all();
        }
        activated
    }

    pub(in crate::player::backend::ffmpeg) fn play_committed_audio_output(
        &self,
    ) -> std::result::Result<(), String> {
        self.play_stream()
    }

    pub(in crate::player::backend::ffmpeg) fn stream_active(&self) -> bool {
        self.stream_active.load(Ordering::Acquire)
    }

    fn play_stream(&self) -> std::result::Result<(), String> {
        if self
            .stream_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }
        if let Some(stream) = self._stream.as_ref()
            && let Err(error) = stream.play()
        {
            self.stream_active.store(false, Ordering::Release);
            return Err(format!("启动系统音频输出流失败：{error}"));
        }
        self.stream_play_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn pause_stream(&self) -> std::result::Result<(), String> {
        if self
            .stream_active
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }
        if let Some(stream) = self._stream.as_ref()
            && let Err(error) = stream.pause()
        {
            self.stream_active.store(true, Ordering::Release);
            return Err(format!("暂停系统音频输出流失败：{error}"));
        }
        self.stream_pause_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn stream_active_for_test(&self) -> bool {
        self.stream_active()
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn stream_control_counts_for_test(&self) -> (u64, u64) {
        (
            self.stream_play_count.load(Ordering::Acquire),
            self.stream_pause_count.load(Ordering::Acquire),
        )
    }

    pub(in crate::player::backend::ffmpeg) fn activate_current_audio_output(
        &self,
        control: &FfmpegControl,
    ) -> bool {
        self.activate_audio_output(self.audio_epoch(), control.seek_generation(), control)
    }

    /// Roll back staged queue ownership without ever waiting for the queue or
    /// callback. `None` means the queue was busy; the epoch fence still makes
    /// every old publication stale so coordinator progress remains bounded.
    pub(in crate::player::backend::ffmpeg) fn try_abort_staged_audio(
        &self,
        expected_epoch: u64,
        reset_timeline_nsecs: u64,
    ) -> std::result::Result<Option<Vec<AudioStagedFrame>>, String> {
        self.timeline.set_active(false);
        let mut state = match self.queue.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => {
                let epoch = self.fence_clock_without_wait(reset_timeline_nsecs);
                self.try_publish_fenced_reset(reset_timeline_nsecs, epoch);
                return Ok(None);
            }
            Err(TryLockError::Poisoned(_)) => {
                let epoch = self.fence_clock_without_wait(reset_timeline_nsecs);
                self.try_publish_fenced_reset(reset_timeline_nsecs, epoch);
                return Err("系统音频解码队列已损坏".to_string());
            }
        };
        if self.audio_epoch() != expected_epoch {
            return Ok(Some(Vec::new()));
        }
        let staged = state
            .items
            .drain(..)
            .filter(|item| item.generation == expected_epoch)
            .map(|item| AudioStagedFrame {
                samples: item.samples,
                start_timeline_nsecs: item.start_timeline_nsecs,
                end_timeline_nsecs: item.end_timeline_nsecs,
            })
            .collect::<Vec<_>>();
        state.clear();
        drop(state);
        let epoch = self.fence_clock_without_wait(reset_timeline_nsecs);
        self.try_publish_fenced_reset(reset_timeline_nsecs, epoch);
        Ok(Some(staged))
    }

    fn try_publish_fenced_reset(&self, timeline_nsecs: u64, epoch: u64) -> bool {
        let mut buffer = match self.shared.buffer.try_lock() {
            Ok(buffer) => buffer,
            Err(_) => return false,
        };
        let _publish = match self.shared.callback_publish_guard.try_lock() {
            Ok(publish) => publish,
            Err(_) => return false,
        };
        buffer.clear();
        buffer.epoch = epoch;
        self.shared.set_queued_end_timeline_nsecs(timeline_nsecs);
        self.shared.played_samples.store(
            audio_elements_for_duration_floor(timeline_nsecs, self.sample_rate, self.channels),
            Ordering::Release,
        );
        self.shared.update_output_delay_unfenced(Duration::ZERO);
        self.shared.clear_underrun();
        true
    }

    pub(in crate::player::backend::ffmpeg) fn underrun_active(&self) -> bool {
        self.shared.underrun_active.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn mark_underrun_for_test(&self, timeline_nsecs: u64) {
        self.shared.mark_underrun(timeline_nsecs);
    }

    pub(in crate::player::backend::ffmpeg) fn drain_deadline(
        &self,
    ) -> std::result::Result<Option<Instant>, String> {
        let timeout = Duration::from_nanos(self.snapshot()?.total_pending_nsecs)
            .saturating_add(Duration::from_millis(250));
        Ok(Instant::now().checked_add(timeout))
    }

    pub(in crate::player::backend::ffmpeg) fn drain_step(
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

    pub(in crate::player::backend::ffmpeg) fn snapshot(
        &self,
    ) -> std::result::Result<AudioOutputSnapshot, String> {
        let _stage = self.begin_service_stage(AudioOutputServiceStage::StatusSnapshot);
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
        let snapshot = compose_audio_output_snapshot(
            shared,
            queue,
            self.timeline.epoch(),
            self.timeline.active(),
            self.timeline.stale_queue_items(),
            self.timeline.stale_callback_publications(),
            None,
        );
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

    /// A coordinator-facing status probe. Lock contention is a retryable AO
    /// Busy result rather than permission to wait behind the callback/worker.
    pub(in crate::player::backend::ffmpeg) fn try_snapshot(
        &self,
    ) -> std::result::Result<Option<AudioOutputSnapshot>, String> {
        let _stage = self.begin_service_stage(AudioOutputServiceStage::StatusSnapshot);
        let Some(mut shared) = self.shared.try_snapshot()? else {
            return Ok(None);
        };
        let Some(queue) = self.queue.try_snapshot()? else {
            return Ok(None);
        };
        let total_pending_nsecs = shared.pending_nsecs.saturating_add(queue.pending_nsecs);
        if self.shared.underrun_active.load(Ordering::Acquire) {
            self.shared.clear_underrun_if_recovered(total_pending_nsecs);
            if !self.shared.underrun_active.load(Ordering::Acquire) {
                let Some(rechecked) = self.shared.try_snapshot()? else {
                    return Ok(None);
                };
                shared = rechecked;
            }
        }
        Ok(Some(compose_audio_output_snapshot(
            shared,
            queue,
            self.timeline.epoch(),
            self.timeline.active(),
            self.timeline.stale_queue_items(),
            self.timeline.stale_callback_publications(),
            None,
        )))
    }

    pub(in crate::player::backend::ffmpeg) fn stable_snapshot(
        &self,
    ) -> std::result::Result<AudioOutputStableSnapshot, String> {
        let _stage = self.begin_service_stage(AudioOutputServiceStage::StableSnapshot);
        self.stable_snapshot_untracked()
    }

    pub(in crate::player::backend::ffmpeg) fn prepared_snapshot(
        &self,
    ) -> std::result::Result<AudioOutputStableSnapshot, String> {
        let _stage = self.begin_service_stage(AudioOutputServiceStage::PreparedSnapshot);
        self.stable_snapshot_untracked()
    }

    fn stable_snapshot_untracked(&self) -> std::result::Result<AudioOutputStableSnapshot, String> {
        stable_audio_output_snapshot_from_parts(
            &self.shared,
            &self.queue,
            &self.timeline,
            || {},
            || {},
            || {},
        )
    }

    pub(in crate::player::backend::ffmpeg) fn begin_service_stage(
        &self,
        stage: AudioOutputServiceStage,
    ) -> AudioOutputServiceStageGuard {
        self.service_telemetry.begin(stage)
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn service_stage_snapshots_for_test(
        &self,
    ) -> [AudioOutputServiceStageSnapshot; 6] {
        self.service_telemetry.snapshots()
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn hold_internal_locks_until_for_test(
        &self,
        entered: std::sync::mpsc::Sender<()>,
        release: &std::sync::atomic::AtomicBool,
    ) {
        let _queue = self
            .queue
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _buffer = self
            .shared
            .buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = entered.send(());
        while !release.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn reset_would_block_for_test(&self) -> bool {
        let queue_busy = self.queue.state.try_lock().is_err();
        let buffer_busy = self.shared.buffer.try_lock().is_err();
        queue_busy || buffer_busy
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn invoke_callback_for_test(&self, samples: &mut [f32]) {
        fill_audio_output(samples, &self.shared);
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn transfer_next_queued_frame_for_test(
        &self,
    ) -> std::result::Result<bool, String> {
        let Some(item) = self.queue.pop()? else {
            return Ok(false);
        };
        let generation = item.generation;
        let samples = item.samples.len();
        let duration_nsecs = item.duration_nsecs;
        let progress = write_audio_queue_item(&self.shared, &self.queue, item)
            .map_err(|error| error.to_string())?;
        let remaining_samples = samples.saturating_sub(progress.samples);
        let remaining_duration_nsecs = duration_nsecs.saturating_sub(progress.duration_nsecs);
        if remaining_samples > 0 || remaining_duration_nsecs > 0 {
            self.queue
                .finish_item(generation, remaining_samples, remaining_duration_nsecs);
        }
        Ok(progress.samples > 0)
    }

    pub(in crate::player::backend::ffmpeg) fn activity_snapshot(
        &self,
    ) -> std::result::Result<AudioOutputActivitySnapshot, String> {
        let shared = self.shared.snapshot()?;
        Ok(AudioOutputActivitySnapshot {
            played_timeline_nsecs: shared.played_timeline_nsecs,
            shared_buffer_pending_nsecs: shared.buffered_nsecs,
            callback_count: self.shared.callback_count.load(Ordering::Acquire),
            consumed_callback_count: self.shared.consumed_callback_count.load(Ordering::Acquire),
            silenced_callback_count: self.shared.silenced_callback_count.load(Ordering::Acquire),
            underrun_count: self.shared.underrun_count.load(Ordering::Acquire),
        })
    }
}

fn stable_audio_output_snapshot_from_parts(
    shared: &AudioShared,
    queue: &AudioQueueShared,
    timeline: &AudioTimelineState,
    mut after_shared_snapshot: impl FnMut(),
    mut before_compose: impl FnMut(),
    mut before_retry: impl FnMut(),
) -> std::result::Result<AudioOutputStableSnapshot, String> {
    let mut observed_epoch;
    let mut observed_version;
    let contention_started_at = Instant::now();
    let mut snapshot_attempts = 0usize;
    let mut contention_retries = 0usize;
    loop {
        if timeline.active_mutations() != 0 {
            observed_epoch = timeline.epoch();
            observed_version = timeline.version();
            if contention_started_at.elapsed() >= AUDIO_OUTPUT_STABLE_SNAPSHOT_CONTENTION_BUDGET {
                break;
            }
            contention_retries = contention_retries.saturating_add(1);
            before_retry();
            // The audio callback owns these mutations for a very short time.
            // Yielding gives it a chance to publish the closing version instead
            // of burning the whole snapshot budget inside one mutation window.
            std::thread::yield_now();
            continue;
        }
        let before_epoch = timeline.epoch();
        let before_version = timeline.version();
        if timeline.active_mutations() != 0 {
            observed_epoch = timeline.epoch();
            observed_version = timeline.version();
            if contention_started_at.elapsed() >= AUDIO_OUTPUT_STABLE_SNAPSHOT_CONTENTION_BUDGET {
                break;
            }
            contention_retries = contention_retries.saturating_add(1);
            before_retry();
            std::thread::yield_now();
            continue;
        }
        snapshot_attempts = snapshot_attempts.saturating_add(1);
        let Some(shared) = shared.try_snapshot()? else {
            observed_epoch = timeline.epoch();
            observed_version = timeline.version();
            if contention_started_at.elapsed() >= AUDIO_OUTPUT_STABLE_SNAPSHOT_CONTENTION_BUDGET {
                break;
            }
            contention_retries = contention_retries.saturating_add(1);
            before_retry();
            std::thread::yield_now();
            continue;
        };
        after_shared_snapshot();
        let Some(queue) = queue.try_snapshot()? else {
            observed_epoch = timeline.epoch();
            observed_version = timeline.version();
            if contention_started_at.elapsed() >= AUDIO_OUTPUT_STABLE_SNAPSHOT_CONTENTION_BUDGET {
                break;
            }
            contention_retries = contention_retries.saturating_add(1);
            before_retry();
            std::thread::yield_now();
            continue;
        };
        let queue_active = timeline.active();
        let stale_queue_items = timeline.stale_queue_items();
        let stale_callback_publications = timeline.stale_callback_publications();
        let after_epoch = timeline.epoch();
        let after_version = timeline.version();
        let after_mutations = timeline.active_mutations();
        observed_epoch = after_epoch;
        observed_version = after_version;
        if after_mutations == 0
            && before_epoch == after_epoch
            && before_version == after_version
            && shared.epoch == after_epoch
            && queue.generation == after_epoch
        {
            before_compose();
            return Ok(AudioOutputStableSnapshot::Stable(
                compose_audio_output_snapshot(
                    shared,
                    queue,
                    after_epoch,
                    queue_active,
                    stale_queue_items,
                    stale_callback_publications,
                    Some(after_version),
                ),
            ));
        }
        if snapshot_attempts >= AUDIO_OUTPUT_STABLE_SNAPSHOT_ATTEMPTS {
            break;
        }
        before_retry();
        std::thread::yield_now();
    }
    Ok(AudioOutputStableSnapshot::SnapshotUnstable(
        AudioOutputUnstableSnapshot {
            audio_epoch: observed_epoch,
            observed_version,
            attempts: snapshot_attempts.saturating_add(contention_retries),
        },
    ))
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg::audio) fn stable_audio_output_snapshot_with_hook_for_test(
    shared: &AudioShared,
    queue: &AudioQueueShared,
    timeline: &AudioTimelineState,
    after_shared_snapshot: impl FnMut(),
) -> std::result::Result<AudioOutputStableSnapshot, String> {
    stable_audio_output_snapshot_from_parts(
        shared,
        queue,
        timeline,
        after_shared_snapshot,
        || {},
        || {},
    )
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg::audio) fn stable_audio_output_snapshot_with_compose_hook_for_test(
    shared: &AudioShared,
    queue: &AudioQueueShared,
    timeline: &AudioTimelineState,
    before_compose: impl FnMut(),
) -> std::result::Result<AudioOutputStableSnapshot, String> {
    stable_audio_output_snapshot_from_parts(shared, queue, timeline, || {}, before_compose, || {})
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg::audio) fn stable_audio_output_snapshot_with_retry_hook_for_test(
    shared: &AudioShared,
    queue: &AudioQueueShared,
    timeline: &AudioTimelineState,
    before_retry: impl FnMut(),
) -> std::result::Result<AudioOutputStableSnapshot, String> {
    stable_audio_output_snapshot_from_parts(shared, queue, timeline, || {}, || {}, before_retry)
}

fn compose_audio_output_snapshot(
    shared: AudioSharedSnapshot,
    queue: AudioQueueSnapshot,
    audio_epoch: u64,
    queue_active: bool,
    stale_queue_items: u64,
    stale_callback_publications: u64,
    stable_version: Option<u64>,
) -> AudioOutputSnapshot {
    let payload_range_nsecs =
        merge_payload_ranges(shared.payload_range_nsecs, queue.payload_range_nsecs);
    let software_payload_nsecs = shared.buffered_nsecs.saturating_add(queue.pending_nsecs);
    let total_pending_nsecs = software_payload_nsecs.saturating_add(shared.output_delay_nsecs);
    AudioOutputSnapshot {
        played_timeline_nsecs: shared.played_timeline_nsecs,
        buffered_until_timeline_nsecs: payload_range_nsecs.map(|(_, end)| end).unwrap_or_else(
            || {
                shared
                    .played_timeline_nsecs
                    .saturating_add(total_pending_nsecs)
            },
        ),
        audio_epoch,
        stable_version,
        shared_payload_nsecs: shared.buffered_nsecs,
        driver_delay_nsecs: shared.output_delay_nsecs,
        shared_pending_nsecs: shared.pending_nsecs,
        queue_pending_nsecs: queue.queued_nsecs,
        worker_in_flight_nsecs: queue.in_flight_nsecs,
        total_pending_nsecs,
        queue_frames: queue.frames,
        worker_in_flight_frames: queue.in_flight_frames,
        queue_generation: queue.generation,
        payload_range_nsecs,
        queue_active,
        stale_queue_items,
        stale_callback_publications,
    }
}

fn merge_payload_ranges(left: Option<(u64, u64)>, right: Option<(u64, u64)>) -> Option<(u64, u64)> {
    match (left, right) {
        (Some((left_start, left_end)), Some((right_start, right_end))) => {
            Some((left_start.min(right_start), left_end.max(right_end)))
        }
        (Some(range), None) | (None, Some(range)) => Some(range),
        (None, None) => None,
    }
}

impl Drop for AudioOutput {
    fn drop(&mut self) {
        self.service_telemetry.shutdown();
        if let Some(handle) = self.service_watchdog.take()
            && handle.join().is_err()
        {
            tracing::debug!("FFmpeg AO service watchdog panicked during shutdown");
        }
        self.queue.shutdown();
        self.shared.ready.notify_all();
        if let Some(handle) = self.queue_worker.take()
            && handle.join().is_err()
        {
            tracing::debug!("FFmpeg audio queue worker panicked during shutdown");
        }
    }
}
