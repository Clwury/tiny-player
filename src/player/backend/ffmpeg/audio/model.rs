use super::{AUDIO_OUTPUT_DELAY_LIMIT, AUDIO_OUTPUT_UNDERRUN_CLOCK_RESUME_DURATION};
use super::{
    Arc, AtomicBool, AtomicU64, AudioOutputServiceTelemetry, Condvar, Duration, FfmpegControl,
    Instant, JoinHandle, Mutex, Ordering, TryLockError, VecDeque, audio_elements_duration,
    audio_elements_for_duration_floor, c_int, duration_nsecs, log_audio_shared_reset_clock_timing,
    log_audio_shared_snapshot_timing,
};

pub(in crate::player::backend::ffmpeg) struct AudioOutput {
    pub(in crate::player::backend::ffmpeg::audio) shared: Arc<AudioShared>,
    pub(in crate::player::backend::ffmpeg::audio) queue: Arc<AudioQueueShared>,
    pub(in crate::player::backend::ffmpeg::audio) timeline: Arc<AudioTimelineState>,
    pub(in crate::player::backend::ffmpeg::audio) queue_worker: Option<JoinHandle<()>>,
    pub(in crate::player::backend::ffmpeg::audio) service_telemetry:
        Arc<AudioOutputServiceTelemetry>,
    pub(in crate::player::backend::ffmpeg::audio) service_watchdog: Option<JoinHandle<()>>,
    pub(in crate::player::backend::ffmpeg::audio) _stream: Option<cpal::Stream>,
    pub(in crate::player::backend::ffmpeg::audio) stream_active: AtomicBool,
    pub(in crate::player::backend::ffmpeg::audio) stream_play_count: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) stream_pause_count: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) pending_fenced_reset_epoch: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) pending_fenced_reset_target_nsecs: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) sample_rate: c_int,
    pub(in crate::player::backend::ffmpeg::audio) channels: c_int,
    pub(in crate::player::backend::ffmpeg::audio) sample_format: String,
    pub(in crate::player::backend::ffmpeg::audio) device_name: String,
}

#[derive(Clone)]
pub(in crate::player::backend::ffmpeg) struct AudioClockHandle {
    shared: Arc<AudioShared>,
    timeline: Arc<AudioTimelineState>,
}

impl AudioClockHandle {
    pub(in crate::player::backend::ffmpeg::audio) fn new(
        shared: Arc<AudioShared>,
        timeline: Arc<AudioTimelineState>,
    ) -> Self {
        Self { shared, timeline }
    }

    pub(in crate::player::backend::ffmpeg) fn played_timeline_nsecs(&self) -> Option<u64> {
        (self.timeline.active() && !self.shared.underrun_active.load(Ordering::Acquire)).then(
            || {
                self.shared
                    .published_played_timeline_nsecs
                    .load(Ordering::Acquire)
            },
        )
    }
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

#[derive(Debug)]
pub(in crate::player::backend::ffmpeg) struct AudioOutputPushError {
    pub(in crate::player::backend::ffmpeg) samples: Vec<f32>,
    pub(in crate::player::backend::ffmpeg) message: String,
}

impl AudioOutputPushError {
    pub(in crate::player::backend::ffmpeg::audio) fn new(
        samples: Vec<f32>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            samples,
            message: message.into(),
        }
    }
}

#[derive(Debug)]
pub(in crate::player::backend::ffmpeg) struct AudioStagedFrame {
    pub(in crate::player::backend::ffmpeg) samples: Vec<f32>,
    pub(in crate::player::backend::ffmpeg) start_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) end_timeline_nsecs: u64,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct AudioOutputSnapshot {
    pub(in crate::player::backend::ffmpeg) played_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) buffered_until_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) audio_epoch: u64,
    pub(in crate::player::backend::ffmpeg) stable_version: Option<u64>,
    pub(in crate::player::backend::ffmpeg) shared_payload_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) driver_delay_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) shared_pending_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) queue_pending_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) worker_in_flight_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) total_pending_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) queue_frames: usize,
    pub(in crate::player::backend::ffmpeg) worker_in_flight_frames: usize,
    pub(in crate::player::backend::ffmpeg) queue_generation: u64,
    pub(in crate::player::backend::ffmpeg) payload_range_nsecs: Option<(u64, u64)>,
    pub(in crate::player::backend::ffmpeg) queue_active: bool,
    pub(in crate::player::backend::ffmpeg) stale_queue_items: u64,
    pub(in crate::player::backend::ffmpeg) stale_callback_publications: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct AudioOutputUnstableSnapshot {
    pub(in crate::player::backend::ffmpeg) audio_epoch: u64,
    pub(in crate::player::backend::ffmpeg) observed_version: u64,
    pub(in crate::player::backend::ffmpeg) attempts: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum AudioOutputStableSnapshot {
    Stable(AudioOutputSnapshot),
    SnapshotUnstable(AudioOutputUnstableSnapshot),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct AudioOutputActivitySnapshot {
    pub(in crate::player::backend::ffmpeg) played_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) shared_buffer_pending_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) callback_count: u64,
    pub(in crate::player::backend::ffmpeg) consumed_callback_count: u64,
    pub(in crate::player::backend::ffmpeg) silenced_callback_count: u64,
    pub(in crate::player::backend::ffmpeg) underrun_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::audio) struct AudioQueueSnapshot {
    pub(in crate::player::backend::ffmpeg::audio) pending_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) queued_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) in_flight_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) frames: usize,
    pub(in crate::player::backend::ffmpeg::audio) in_flight_frames: usize,
    pub(in crate::player::backend::ffmpeg::audio) generation: u64,
    pub(in crate::player::backend::ffmpeg::audio) payload_range_nsecs: Option<(u64, u64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::audio) struct AudioSharedSnapshot {
    pub(in crate::player::backend::ffmpeg::audio) played_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) buffered_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) output_delay_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) pending_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) epoch: u64,
    pub(in crate::player::backend::ffmpeg::audio) payload_range_nsecs: Option<(u64, u64)>,
}

pub(in crate::player::backend::ffmpeg::audio) struct AudioTimelineState {
    epoch: AtomicU64,
    version: AtomicU64,
    active_mutations: AtomicU64,
    active: AtomicBool,
    stale_queue_items: AtomicU64,
    stale_callback_publications: AtomicU64,
}

pub(in crate::player::backend::ffmpeg::audio) struct AudioTimelineMutation<'a> {
    timeline: &'a AudioTimelineState,
}

pub(in crate::player::backend::ffmpeg) struct AudioShared {
    pub(in crate::player::backend::ffmpeg) buffer: Mutex<AudioBuffer>,
    pub(in crate::player::backend::ffmpeg) ready: Condvar,
    pub(in crate::player::backend::ffmpeg::audio) timeline: Arc<AudioTimelineState>,
    pub(in crate::player::backend::ffmpeg::audio) callback_publish_guard: Mutex<()>,
    pub(in crate::player::backend::ffmpeg) played_samples: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) published_played_timeline_nsecs: AtomicU64,
    pub(in crate::player::backend::ffmpeg) queued_end_timeline_nsecs: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) output_delay_nsecs: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) output_delay_updated_nsecs: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) callback_count: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) consumed_callback_count: AtomicU64,
    pub(in crate::player::backend::ffmpeg::audio) silenced_callback_count: AtomicU64,
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
    pub(in crate::player::backend::ffmpeg::audio) epoch: u64,
}

#[derive(Debug)]
pub(in crate::player::backend::ffmpeg::audio) struct AudioQueueItem {
    pub(in crate::player::backend::ffmpeg::audio) samples: Vec<f32>,
    pub(in crate::player::backend::ffmpeg::audio) start_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) end_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) duration_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::audio) struct AudioQueueInFlight {
    pub(in crate::player::backend::ffmpeg::audio) generation: u64,
    pub(in crate::player::backend::ffmpeg::audio) start_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) end_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) remaining_samples: usize,
    pub(in crate::player::backend::ffmpeg::audio) remaining_duration_nsecs: u64,
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
    pub(in crate::player::backend::ffmpeg::audio) timeline: Arc<AudioTimelineState>,
    pub(in crate::player::backend::ffmpeg::audio) shutdown: AtomicBool,
    pub(in crate::player::backend::ffmpeg::audio) control: Arc<FfmpegControl>,
}

pub(in crate::player::backend::ffmpeg::audio) struct AudioQueueState {
    pub(in crate::player::backend::ffmpeg::audio) items: VecDeque<AudioQueueItem>,
    pub(in crate::player::backend::ffmpeg::audio) queued_samples: usize,
    pub(in crate::player::backend::ffmpeg::audio) queued_duration_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::audio) in_flight: Option<AudioQueueInFlight>,
}

impl AudioTimelineState {
    pub(in crate::player::backend::ffmpeg::audio) fn new(active: bool) -> Self {
        Self {
            epoch: AtomicU64::new(0),
            version: AtomicU64::new(0),
            active_mutations: AtomicU64::new(0),
            active: AtomicBool::new(active),
            stale_queue_items: AtomicU64::new(0),
            stale_callback_publications: AtomicU64::new(0),
        }
    }

    pub(in crate::player::backend::ffmpeg::audio) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    pub(in crate::player::backend::ffmpeg::audio) fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    pub(in crate::player::backend::ffmpeg::audio) fn active_mutations(&self) -> u64 {
        self.active_mutations.load(Ordering::Acquire)
    }

    pub(in crate::player::backend::ffmpeg::audio) fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub(in crate::player::backend::ffmpeg::audio) fn begin_mutation(
        &self,
    ) -> AudioTimelineMutation<'_> {
        self.active_mutations.fetch_add(1, Ordering::AcqRel);
        // Publish at mutation entry as well as exit. A stable reader can then
        // detect a writer that started during its two-phase snapshot even if
        // that writer has not completed yet.
        self.version.fetch_add(1, Ordering::AcqRel);
        AudioTimelineMutation { timeline: self }
    }

    pub(in crate::player::backend::ffmpeg::audio) fn advance_epoch(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::AcqRel).saturating_add(1)
    }

    pub(in crate::player::backend::ffmpeg::audio) fn set_active(&self, active: bool) {
        let _mutation = self.begin_mutation();
        self.active.store(active, Ordering::Release);
    }

    pub(in crate::player::backend::ffmpeg::audio) fn activate_if_epoch(
        &self,
        expected_epoch: u64,
    ) -> bool {
        let _mutation = self.begin_mutation();
        if self.epoch() != expected_epoch {
            return false;
        }
        self.active.store(true, Ordering::Release);
        true
    }

    pub(in crate::player::backend::ffmpeg::audio) fn is_current_epoch(&self, epoch: u64) -> bool {
        self.epoch() == epoch
    }

    pub(in crate::player::backend::ffmpeg::audio) fn record_stale_queue_item(&self) {
        self.record_stale_queue_items(1);
    }

    pub(in crate::player::backend::ffmpeg::audio) fn record_stale_queue_items(&self, count: u64) {
        self.stale_queue_items.fetch_add(count, Ordering::Relaxed);
    }

    pub(in crate::player::backend::ffmpeg::audio) fn stale_queue_items(&self) -> u64 {
        self.stale_queue_items.load(Ordering::Acquire)
    }

    pub(in crate::player::backend::ffmpeg::audio) fn record_stale_callback_publication(&self) {
        self.stale_callback_publications
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(in crate::player::backend::ffmpeg::audio) fn stale_callback_publications(&self) -> u64 {
        self.stale_callback_publications.load(Ordering::Acquire)
    }
}

impl Drop for AudioTimelineMutation<'_> {
    fn drop(&mut self) {
        self.timeline.version.fetch_add(1, Ordering::Release);
        self.timeline
            .active_mutations
            .fetch_sub(1, Ordering::Release);
    }
}

impl AudioShared {
    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn new(
        max_samples: usize,
        sample_rate: c_int,
        channels: c_int,
        control: Arc<FfmpegControl>,
    ) -> Self {
        Self::with_timeline(
            max_samples,
            sample_rate,
            channels,
            control,
            Arc::new(AudioTimelineState::new(true)),
        )
    }

    pub(in crate::player::backend::ffmpeg::audio) fn with_timeline(
        max_samples: usize,
        sample_rate: c_int,
        channels: c_int,
        control: Arc<FfmpegControl>,
        timeline: Arc<AudioTimelineState>,
    ) -> Self {
        let epoch = timeline.epoch();
        Self {
            buffer: Mutex::new(AudioBuffer::with_capacity_and_epoch(max_samples, epoch)),
            ready: Condvar::new(),
            timeline,
            callback_publish_guard: Mutex::new(()),
            played_samples: AtomicU64::new(0),
            published_played_timeline_nsecs: AtomicU64::new(0),
            queued_end_timeline_nsecs: AtomicU64::new(0),
            output_delay_nsecs: AtomicU64::new(0),
            output_delay_updated_nsecs: AtomicU64::new(0),
            callback_count: AtomicU64::new(0),
            consumed_callback_count: AtomicU64::new(0),
            silenced_callback_count: AtomicU64::new(0),
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

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn reset_clock(&self, timeline_nsecs: u64) {
        self.timeline.set_active(false);
        let mutation = self.timeline.begin_mutation();
        let epoch = self.timeline.advance_epoch();
        self.reset_clock_at_epoch(timeline_nsecs, epoch);
        drop(mutation);
    }

    pub(in crate::player::backend::ffmpeg::audio) fn reset_clock_at_epoch(
        &self,
        timeline_nsecs: u64,
        epoch: u64,
    ) {
        let started_at = Instant::now();
        let lock_started_at = Instant::now();
        let lock_result = self.buffer.lock();
        let lock_wait = lock_started_at.elapsed();
        let buffer_cleared = lock_result.is_ok();
        if let Ok(mut guard) = lock_result {
            guard.clear();
            guard.epoch = epoch;
            self.queued_end_timeline_nsecs
                .store(timeline_nsecs, Ordering::Release);
            self.ready.notify_all();
        }
        let callback_guard = self
            .callback_publish_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.played_samples.store(
            audio_elements_for_duration_floor(timeline_nsecs, self.sample_rate, self.channels),
            Ordering::Release,
        );
        self.published_played_timeline_nsecs
            .store(timeline_nsecs, Ordering::Release);
        self.update_output_delay_unfenced(Duration::ZERO);
        self.clear_underrun();
        drop(callback_guard);
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
            .store(timeline_nsecs, Ordering::Release);
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

    pub(in crate::player::backend::ffmpeg::audio) fn update_output_delay_unfenced(
        &self,
        delay: Duration,
    ) {
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
    pub(in crate::player::backend::ffmpeg) fn activate_for_test(&self) {
        self.timeline.set_active(true);
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
        let guard = self
            .buffer
            .lock()
            .map_err(|_| "系统音频缓冲区已损坏".to_string())?;
        let queued_samples = guard.len();
        let buffer_lock_wait = lock_started_at.elapsed();
        let snapshot = self.snapshot_for_locked_buffer(&guard);
        drop(guard);
        log_audio_shared_snapshot_timing(
            started_at.elapsed(),
            buffer_lock_wait,
            queued_samples,
            snapshot,
        );
        Ok(snapshot)
    }

    pub(in crate::player::backend::ffmpeg::audio) fn try_snapshot(
        &self,
    ) -> std::result::Result<Option<AudioSharedSnapshot>, String> {
        let guard = match self.buffer.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Poisoned(_)) => {
                return Err("系统音频缓冲区已损坏".to_string());
            }
        };
        Ok(Some(self.snapshot_for_locked_buffer(&guard)))
    }

    fn snapshot_for_locked_buffer(&self, guard: &AudioBuffer) -> AudioSharedSnapshot {
        let queued_samples = guard.len();
        let epoch = guard.epoch;
        let queued_end_timeline_nsecs = self.queued_end_timeline_nsecs.load(Ordering::Acquire);
        let queued_duration_nsecs = duration_nsecs(audio_elements_duration(
            queued_samples,
            self.sample_rate,
            self.channels,
        ));
        let output_delay_nsecs = self.output_delay_nsecs();
        let pending_nsecs = queued_duration_nsecs.saturating_add(output_delay_nsecs);
        let played_timeline_nsecs = self.played_timeline_nsecs_for_pending(queued_duration_nsecs);
        AudioSharedSnapshot {
            played_timeline_nsecs,
            buffered_nsecs: queued_duration_nsecs,
            output_delay_nsecs,
            pending_nsecs,
            epoch,
            payload_range_nsecs: (queued_duration_nsecs > 0).then_some((
                queued_end_timeline_nsecs.saturating_sub(queued_duration_nsecs),
                queued_end_timeline_nsecs,
            )),
        }
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn set_output_delay_for_test(&self, delay: Duration) {
        let _guard = self
            .callback_publish_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.output_delay_nsecs
            .store(duration_nsecs(delay), Ordering::Relaxed);
        self.output_delay_updated_nsecs.store(
            duration_nsecs(self.clock_start.elapsed()).saturating_add(1_000_000_000),
            Ordering::Relaxed,
        );
    }
}
