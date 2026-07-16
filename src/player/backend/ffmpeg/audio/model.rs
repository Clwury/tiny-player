use super::{AUDIO_OUTPUT_DELAY_LIMIT, AUDIO_OUTPUT_UNDERRUN_CLOCK_RESUME_DURATION};
use super::{
    Arc, AtomicBool, AtomicU64, Condvar, Duration, FfmpegControl, Instant, JoinHandle, Mutex,
    Ordering, VecDeque, audio_elements_duration, audio_elements_for_duration_floor, c_int,
    duration_nsecs, log_audio_shared_reset_clock_timing, log_audio_shared_snapshot_timing,
};

pub(in crate::player::backend::ffmpeg) struct AudioOutput {
    pub(in crate::player::backend::ffmpeg::audio) shared: Arc<AudioShared>,
    pub(in crate::player::backend::ffmpeg::audio) queue: Arc<AudioQueueShared>,
    pub(in crate::player::backend::ffmpeg::audio) queue_worker: Option<JoinHandle<()>>,
    pub(in crate::player::backend::ffmpeg::audio) _stream: cpal::Stream,
    pub(in crate::player::backend::ffmpeg::audio) sample_rate: c_int,
    pub(in crate::player::backend::ffmpeg::audio) channels: c_int,
    pub(in crate::player::backend::ffmpeg::audio) sample_format: String,
    pub(in crate::player::backend::ffmpeg::audio) device_name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum AudioOutputDrainStatus {
    Drained,
    Waiting,
    Interrupted,
}

pub(in crate::player::backend::ffmpeg) enum AudioOutputPushResult {
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
pub(in crate::player::backend::ffmpeg) enum AudioClockMode {
    SyncingVideo,
    AudioStarted,
    UnderrunRecovery,
}

impl AudioClockMode {
    pub(in crate::player::backend::ffmpeg) fn as_str(self) -> &'static str {
        match self {
            Self::SyncingVideo => "syncing_video",
            Self::AudioStarted => "audio_started",
            Self::UnderrunRecovery => "underrun_recovery",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct AudioOutputSnapshot {
    pub(in crate::player::backend::ffmpeg) played_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) buffered_until_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) shared_pending_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) queue_pending_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) total_pending_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) queue_frames: usize,
    pub(in crate::player::backend::ffmpeg) queue_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::audio) struct AudioQueueSnapshot {
    pub(in crate::player::backend::ffmpeg::audio) pending_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) frames: usize,
    pub(in crate::player::backend::ffmpeg::audio) generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::audio) struct AudioSharedSnapshot {
    pub(in crate::player::backend::ffmpeg::audio) played_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) pending_nsecs: u64,
}

pub(in crate::player::backend::ffmpeg) struct AudioShared {
    pub(in crate::player::backend::ffmpeg) buffer: Mutex<AudioBuffer>,
    pub(in crate::player::backend::ffmpeg) ready: Condvar,
    pub(in crate::player::backend::ffmpeg) played_samples: AtomicU64,
    pub(in crate::player::backend::ffmpeg) queued_end_timeline_nsecs: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) output_delay_nsecs: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) output_delay_updated_nsecs: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) callback_count: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) underrun_count: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) underrun_active: AtomicBool,
    pub(in crate::player::backend::ffmpeg::audio) underrun_timeline_nsecs: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) misaligned_audio_buffer_count: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) last_callback_nsecs: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) clock_start: Instant,
    pub(in crate::player::backend::ffmpeg::audio) sample_rate: c_int,
    pub(in crate::player::backend::ffmpeg::audio) channels: c_int,
    pub(in crate::player::backend::ffmpeg) control: Arc<FfmpegControl>,
}

pub(in crate::player::backend::ffmpeg) struct AudioBuffer {
    pub(in crate::player::backend::ffmpeg::audio) samples: Vec<f32>,
    pub(in crate::player::backend::ffmpeg::audio) read_pos: usize,
    pub(in crate::player::backend::ffmpeg::audio) write_pos: usize,
    pub(in crate::player::backend::ffmpeg::audio) len: usize,
}

pub(in crate::player::backend::ffmpeg::audio) struct AudioQueueItem {
    pub(in crate::player::backend::ffmpeg::audio) samples: Vec<f32>,
    pub(in crate::player::backend::ffmpeg::audio) start_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) end_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) duration_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) generation: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::audio) struct AudioQueueWriteProgress {
    pub(in crate::player::backend::ffmpeg::audio) samples: usize,
    pub(in crate::player::backend::ffmpeg::audio) duration_nsecs: u64,
}

#[derive(Debug)]
pub(in crate::player::backend::ffmpeg::audio) struct AudioQueueWriteError {
    pub(in crate::player::backend::ffmpeg::audio) message: String,
    pub(in crate::player::backend::ffmpeg::audio) progress: AudioQueueWriteProgress,
}

impl AudioQueueWriteError {
    pub(in crate::player::backend::ffmpeg::audio) fn new(
        message: impl Into<String>,
        progress: AudioQueueWriteProgress,
    ) -> Self {
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

pub(in crate::player::backend::ffmpeg::audio) struct AudioQueueShared {
    pub(in crate::player::backend::ffmpeg::audio) state: Mutex<AudioQueueState>,
    pub(in crate::player::backend::ffmpeg::audio) ready: Condvar,
    pub(in crate::player::backend::ffmpeg::audio) generation: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) shutdown: AtomicBool,
    pub(in crate::player::backend::ffmpeg::audio) control: Arc<FfmpegControl>,
}

pub(in crate::player::backend::ffmpeg::audio) struct AudioQueueState {
    pub(in crate::player::backend::ffmpeg::audio) items: VecDeque<AudioQueueItem>,
    pub(in crate::player::backend::ffmpeg::audio) queued_samples: usize,
    pub(in crate::player::backend::ffmpeg::audio) queued_duration_nsecs: u64,
}

impl AudioShared {
    pub(in crate::player::backend::ffmpeg) fn new(
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

    pub(in crate::player::backend::ffmpeg) fn reset_clock(&self, timeline_nsecs: u64) {
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

    pub(in crate::player::backend::ffmpeg) fn set_queued_end_timeline_nsecs(
        &self,
        timeline_nsecs: u64,
    ) {
        self.queued_end_timeline_nsecs
            .store(timeline_nsecs, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::audio) fn queued_duration(
        &self,
    ) -> std::result::Result<Duration, String> {
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
    pub(in crate::player::backend::ffmpeg::audio) fn queued_duration_nsecs(&self) -> u64 {
        self.queued_duration()
            .map(duration_nsecs)
            .unwrap_or_default()
    }

    pub(in crate::player::backend::ffmpeg::audio) fn output_delay_nsecs(&self) -> u64 {
        let delay = self.output_delay_nsecs.load(Ordering::Relaxed);
        if delay == 0 {
            return 0;
        }
        let updated = self.output_delay_updated_nsecs.load(Ordering::Relaxed);
        let elapsed = duration_nsecs(self.clock_start.elapsed()).saturating_sub(updated);
        delay.saturating_sub(elapsed)
    }

    pub(in crate::player::backend::ffmpeg::audio) fn update_output_delay(&self, delay: Duration) {
        let delay = delay.min(AUDIO_OUTPUT_DELAY_LIMIT);
        self.output_delay_nsecs
            .store(duration_nsecs(delay), Ordering::Relaxed);
        self.output_delay_updated_nsecs.store(
            duration_nsecs(self.clock_start.elapsed()),
            Ordering::Relaxed,
        );
    }

    pub(in crate::player::backend::ffmpeg::audio) fn played_timeline_nsecs_for_pending(
        &self,
        pending_nsecs: u64,
    ) -> u64 {
        if self.underrun_active.load(Ordering::Acquire) {
            return self.underrun_timeline_nsecs.load(Ordering::Acquire);
        }
        self.played_timeline_nsecs_from_pending(pending_nsecs)
    }

    pub(in crate::player::backend::ffmpeg::audio) fn played_timeline_nsecs_from_pending(
        &self,
        pending_nsecs: u64,
    ) -> u64 {
        self.queued_end_timeline_nsecs
            .load(Ordering::Relaxed)
            .saturating_sub(pending_nsecs)
            .saturating_sub(self.output_delay_nsecs())
    }

    pub(in crate::player::backend::ffmpeg::audio) fn mark_underrun(
        &self,
        played_timeline_nsecs: u64,
    ) -> bool {
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

    pub(in crate::player::backend::ffmpeg::audio) fn clear_underrun(&self) {
        self.underrun_active.store(false, Ordering::Release);
    }

    pub(in crate::player::backend::ffmpeg::audio) fn clear_underrun_if_recovered(
        &self,
        pending_nsecs: u64,
    ) {
        // Keep the 250 ms watermark for low-water admission and rebuffer
        // planning, but release the frozen audio/video clock sooner once a
        // contiguous 120 ms AO window has been rebuilt.
        if pending_nsecs >= duration_nsecs(AUDIO_OUTPUT_UNDERRUN_CLOCK_RESUME_DURATION) {
            self.clear_underrun();
        }
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn underrun_active_for_test(&self) -> bool {
        self.underrun_active.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn clear_underrun_if_recovered_for_test(
        &self,
        pending_nsecs: u64,
    ) {
        self.clear_underrun_if_recovered(pending_nsecs);
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn played_timeline_nsecs(&self) -> u64 {
        self.played_timeline_nsecs_for_pending(self.queued_duration_nsecs())
    }

    pub(in crate::player::backend::ffmpeg::audio) fn snapshot(
        &self,
    ) -> std::result::Result<AudioSharedSnapshot, String> {
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
    pub(in crate::player::backend::ffmpeg) fn set_output_delay_for_test(&self, delay: Duration) {
        self.output_delay_nsecs
            .store(duration_nsecs(delay), Ordering::Relaxed);
        self.output_delay_updated_nsecs.store(
            duration_nsecs(self.clock_start.elapsed()).saturating_add(1_000_000_000),
            Ordering::Relaxed,
        );
    }
}
