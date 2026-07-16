use super::{
    AUDIO_BUFFER_SECONDS, Arc, AudioOutput, AudioOutputDrainStatus, AudioOutputPushResult,
    AudioOutputSnapshot, AudioOutputSnapshotTiming, AudioOutputTryPushTimedTiming, AudioQueueItem,
    AudioQueueShared, AudioShared, DeviceTrait, Duration, FfmpegControl, Instant, Ordering,
    StreamTrait, align_audio_elements_to_frame_boundary, build_audio_output_stream, c_int,
    log_audio_output_reset_clock_timing, log_audio_output_snapshot_timing,
    log_audio_output_try_push_timed_timing, output_device_candidates, spawn_audio_queue_worker,
};

impl AudioOutput {
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
        let shared = Arc::new(AudioShared::new(
            max_samples,
            sample_rate,
            channels,
            Arc::clone(&control),
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
            sample_format: sample_format_name,
            device_name,
        })
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

    pub(in crate::player::backend::ffmpeg) fn reset_clock(&self, timeline_nsecs: u64) {
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

    pub(in crate::player::backend::ffmpeg) fn underrun_active(&self) -> bool {
        self.shared.underrun_active.load(Ordering::Acquire)
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
