use std::{
    any::Any,
    os::raw::{c_int, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::player::render_host::{PlaybackSessionId, VideoOutputQueue};

use super::{
    BackendError, BackendEvent, BackendEventKind, PLAYBACK_VOLUME_SCALE, PlaybackCacheConfig,
    PlaybackSeekMode, Result, normalize_playback_volume,
};
#[cfg(test)]
use super::{DEFAULT_PLAYBACK_VOLUME, SCHEDULER_POLL_INTERVAL};

pub(super) struct FfmpegWorker {
    control: Arc<FfmpegControl>,
    command_tx: Sender<FfmpegCommand>,
    handle: JoinHandle<()>,
}

const AUDIO_OUTPUT_LIFECYCLE_MASK: u32 = 0b11;
const AUDIO_OUTPUT_PAUSED_BY_USER: u32 = 1 << 8;
const AUDIO_OUTPUT_PAUSED_BY_CACHE: u32 = 1 << 9;
const AUDIO_OUTPUT_PAUSED_BY_REBUFFER: u32 = 1 << 10;
const AUDIO_OUTPUT_PAUSED_BY_SEEK_TRANSITION: u32 = 1 << 11;
const AUDIO_OUTPUT_PAUSE_MASK: u32 = AUDIO_OUTPUT_PAUSED_BY_USER
    | AUDIO_OUTPUT_PAUSED_BY_CACHE
    | AUDIO_OUTPUT_PAUSED_BY_REBUFFER
    | AUDIO_OUTPUT_PAUSED_BY_SEEK_TRANSITION;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u32)]
pub(super) enum AudioOutputLifecycle {
    #[default]
    Syncing = 0,
    Ready = 1,
    Playing = 2,
    Draining = 3,
}

impl AudioOutputLifecycle {
    fn from_state_word(state_word: u32) -> Self {
        match state_word & AUDIO_OUTPUT_LIFECYCLE_MASK {
            0 => Self::Syncing,
            1 => Self::Ready,
            2 => Self::Playing,
            3 => Self::Draining,
            _ => unreachable!("audio output lifecycle mask is two bits"),
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Syncing => "syncing",
            Self::Ready => "ready",
            Self::Playing => "playing",
            Self::Draining => "draining",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AudioOutputDecision {
    Consume,
    Silence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AudioOutputControlSnapshot {
    state_word: u32,
}

impl AudioOutputControlSnapshot {
    fn new(state_word: u32) -> Self {
        Self { state_word }
    }

    pub(super) fn lifecycle(self) -> AudioOutputLifecycle {
        AudioOutputLifecycle::from_state_word(self.state_word)
    }

    pub(super) fn paused_by_user(self) -> bool {
        self.state_word & AUDIO_OUTPUT_PAUSED_BY_USER != 0
    }

    pub(super) fn paused_by_cache(self) -> bool {
        self.state_word & AUDIO_OUTPUT_PAUSED_BY_CACHE != 0
    }

    pub(super) fn paused_by_rebuffer(self) -> bool {
        self.state_word & AUDIO_OUTPUT_PAUSED_BY_REBUFFER != 0
    }

    pub(super) fn paused_by_seek_transition(self) -> bool {
        self.state_word & AUDIO_OUTPUT_PAUSED_BY_SEEK_TRANSITION != 0
    }

    pub(super) fn externally_paused(self) -> bool {
        self.state_word
            & (AUDIO_OUTPUT_PAUSED_BY_USER
                | AUDIO_OUTPUT_PAUSED_BY_CACHE
                | AUDIO_OUTPUT_PAUSED_BY_REBUFFER)
            != 0
    }

    pub(super) fn decision(self) -> AudioOutputDecision {
        let lifecycle_consumes = matches!(
            self.lifecycle(),
            AudioOutputLifecycle::Playing | AudioOutputLifecycle::Draining
        );
        if lifecycle_consumes && self.state_word & AUDIO_OUTPUT_PAUSE_MASK == 0 {
            AudioOutputDecision::Consume
        } else {
            AudioOutputDecision::Silence
        }
    }
}

#[derive(Debug)]
pub(super) struct FfmpegControl {
    shutdown: AtomicBool,
    audio_output_state: AtomicU32,
    seek_transition_guard: Mutex<()>,
    volume: AtomicU32,
    session_id: AtomicU64,
    seek_generation: AtomicU64,
    handled_seek_generation: AtomicU64,
    output_underrun_for_cache_pause: AtomicBool,
    wake_generation: AtomicU64,
    wake_guard: Mutex<()>,
    wake_ready: Condvar,
}

impl FfmpegControl {
    #[cfg(test)]
    pub(super) fn new(session_id: PlaybackSessionId) -> Self {
        Self::with_volume(session_id, DEFAULT_PLAYBACK_VOLUME)
    }

    pub(super) fn with_volume(session_id: PlaybackSessionId, volume: f32) -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            audio_output_state: AtomicU32::new(AudioOutputLifecycle::Syncing as u32),
            seek_transition_guard: Mutex::new(()),
            volume: AtomicU32::new(volume_to_storage(volume)),
            session_id: AtomicU64::new(session_id.0),
            seek_generation: AtomicU64::new(0),
            handled_seek_generation: AtomicU64::new(0),
            output_underrun_for_cache_pause: AtomicBool::new(false),
            wake_generation: AtomicU64::new(0),
            wake_guard: Mutex::new(()),
            wake_ready: Condvar::new(),
        }
    }

    pub(super) fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.wake();
    }

    pub(super) fn should_stop(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    pub(super) fn should_interrupt(&self) -> bool {
        self.should_stop() || self.has_pending_seek()
    }

    pub(super) fn is_paused(&self) -> bool {
        let state = self.audio_output_control_snapshot();
        state.paused_by_user() || state.paused_by_cache()
    }

    pub(super) fn audio_output_control_snapshot(&self) -> AudioOutputControlSnapshot {
        AudioOutputControlSnapshot::new(self.audio_output_state.load(Ordering::Acquire))
    }

    pub(super) fn audio_output_lifecycle(&self) -> AudioOutputLifecycle {
        self.audio_output_control_snapshot().lifecycle()
    }

    pub(super) fn set_audio_output_lifecycle(&self, lifecycle: AudioOutputLifecycle) -> bool {
        if self.audio_output_lifecycle() == lifecycle {
            return false;
        }
        let _guard = self
            .seek_transition_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if lifecycle == AudioOutputLifecycle::Ready && self.has_pending_seek() {
            return false;
        }
        if matches!(
            lifecycle,
            AudioOutputLifecycle::Playing | AudioOutputLifecycle::Draining
        ) && (self.has_pending_seek()
            || self
                .audio_output_control_snapshot()
                .paused_by_seek_transition())
        {
            return false;
        }
        let (previous, current) = self.update_audio_output_state(|state| {
            (state & !AUDIO_OUTPUT_LIFECYCLE_MASK) | lifecycle as u32
        });
        let changed = previous != current;
        if changed {
            self.wake();
        }
        changed
    }

    pub(super) fn is_user_paused(&self) -> bool {
        self.audio_output_control_snapshot().paused_by_user()
    }

    pub(super) fn set_user_paused(&self, paused: bool) {
        self.set_audio_output_pause_reason(AUDIO_OUTPUT_PAUSED_BY_USER, paused);
    }

    pub(super) fn is_cache_paused(&self) -> bool {
        self.audio_output_control_snapshot().paused_by_cache()
    }

    pub(super) fn set_cache_paused(&self, paused: bool) -> bool {
        self.set_audio_output_pause_reason(AUDIO_OUTPUT_PAUSED_BY_CACHE, paused)
    }

    pub(super) fn set_output_underrun_for_cache_pause(&self, underrun: bool) {
        if self
            .output_underrun_for_cache_pause
            .swap(underrun, Ordering::AcqRel)
            != underrun
        {
            self.wake();
        }
    }

    pub(super) fn output_underrun_for_cache_pause(&self) -> bool {
        self.output_underrun_for_cache_pause.load(Ordering::Acquire)
    }

    pub(super) fn is_output_rebuffer_paused(&self) -> bool {
        self.audio_output_control_snapshot().paused_by_rebuffer()
    }

    pub(super) fn set_output_rebuffer_paused(&self, paused: bool) -> bool {
        self.set_audio_output_pause_reason(AUDIO_OUTPUT_PAUSED_BY_REBUFFER, paused)
    }

    #[cfg(test)]
    pub(super) fn is_seek_audio_paused(&self) -> bool {
        self.audio_output_control_snapshot()
            .paused_by_seek_transition()
    }

    /// Keep the native callback in an explicit seek-silence state until the
    /// new target has produced an A/V pair. This is intentionally separate
    /// from `has_pending_seek`: the command can be acknowledged before the
    /// decoder/output queues are ready to restart the audio clock.
    pub(super) fn finish_seek_audio_pause(&self) -> bool {
        if !self
            .audio_output_control_snapshot()
            .paused_by_seek_transition()
        {
            return false;
        }
        let _guard = self
            .seek_transition_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.has_pending_seek() {
            return false;
        }
        let (previous, _) =
            self.update_audio_output_state(|state| state & !AUDIO_OUTPUT_PAUSED_BY_SEEK_TRANSITION);
        let changed = previous & AUDIO_OUTPUT_PAUSED_BY_SEEK_TRANSITION != 0;
        if changed {
            self.wake();
        }
        changed
    }

    /// Atomically validates the seek generation, activates the matching AO
    /// epoch, clears seek silence, and publishes Playing while holding the
    /// same guard used by `request_seek`.
    pub(super) fn compare_and_commit_audio_output_start(
        &self,
        expected_seek_generation: u64,
        activate_epoch: impl FnOnce() -> bool,
    ) -> bool {
        let _guard = self
            .seek_transition_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.should_stop()
            || self.seek_generation.load(Ordering::Acquire) != expected_seek_generation
            || self.handled_seek_generation.load(Ordering::Acquire) != expected_seek_generation
        {
            return false;
        }
        if !activate_epoch() {
            return false;
        }
        self.update_audio_output_state(|state| {
            (state & !(AUDIO_OUTPUT_LIFECYCLE_MASK | AUDIO_OUTPUT_PAUSED_BY_SEEK_TRANSITION))
                | AudioOutputLifecycle::Playing as u32
        });
        self.wake();
        true
    }

    fn set_audio_output_pause_reason(&self, reason: u32, paused: bool) -> bool {
        let (previous, current) = self.update_audio_output_state(|state| {
            if paused {
                state | reason
            } else {
                state & !reason
            }
        });
        let changed = previous != current;
        if changed {
            self.wake();
        }
        changed
    }

    fn update_audio_output_state(&self, mut update: impl FnMut(u32) -> u32) -> (u32, u32) {
        let mut previous = self.audio_output_state.load(Ordering::Acquire);
        loop {
            let current = update(previous);
            if current == previous {
                return (previous, current);
            }
            match self.audio_output_state.compare_exchange_weak(
                previous,
                current,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return (previous, current),
                Err(observed) => previous = observed,
            }
        }
    }

    pub(super) fn set_volume(&self, volume: f32) {
        self.volume
            .store(volume_to_storage(volume), Ordering::Release);
    }

    pub(super) fn volume(&self) -> f32 {
        self.volume.load(Ordering::Acquire) as f32 / PLAYBACK_VOLUME_SCALE as f32
    }

    #[cfg(test)]
    pub(super) fn wait_while_paused(&self) -> bool {
        while self.is_paused() && !self.should_stop() && !self.has_pending_seek() {
            thread::sleep(SCHEDULER_POLL_INTERVAL);
        }
        self.should_stop()
    }

    pub(super) fn session_id(&self) -> PlaybackSessionId {
        PlaybackSessionId(self.session_id.load(Ordering::Acquire))
    }

    pub(super) fn set_session_id(&self, session_id: PlaybackSessionId) {
        self.session_id.store(session_id.0, Ordering::Release);
    }

    pub(super) fn request_seek(&self) -> u64 {
        // Serialize generation publication with transition completion so the
        // callback-visible seek-silence bit can be raised before the new seek
        // becomes interruptible, without a completion racing to clear it.
        let generation = {
            let _guard = self
                .seek_transition_guard
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.update_audio_output_state(|state| {
                (state & !(AUDIO_OUTPUT_LIFECYCLE_MASK | AUDIO_OUTPUT_PAUSED_BY_REBUFFER))
                    | AudioOutputLifecycle::Syncing as u32
                    | AUDIO_OUTPUT_PAUSED_BY_SEEK_TRANSITION
            });
            self.output_underrun_for_cache_pause
                .store(false, Ordering::Release);
            self.seek_generation.fetch_add(1, Ordering::AcqRel) + 1
        };
        // Publish the interrupt only after releasing the seek transition
        // guard, so a woken coordinator can immediately enter command service.
        self.wake();
        generation
    }

    pub(super) fn seek_generation(&self) -> u64 {
        self.seek_generation.load(Ordering::Acquire)
    }

    pub(super) fn finish_seek(&self, generation: u64) {
        let mut current = self.handled_seek_generation.load(Ordering::Acquire);
        while generation > current {
            match self.handled_seek_generation.compare_exchange_weak(
                current,
                generation,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.wake();
                    return;
                }
                Err(next) => current = next,
            }
        }
    }

    pub(super) fn has_pending_seek(&self) -> bool {
        self.seek_generation.load(Ordering::Acquire)
            > self.handled_seek_generation.load(Ordering::Acquire)
    }

    /// Publish a state transition to every playback wait domain. The mutex is
    /// intentionally shared by publishers and waiters so a notification
    /// cannot be lost between the generation check and `Condvar::wait`.
    pub(super) fn wake(&self) -> u64 {
        let _guard = self
            .wake_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let generation = self.wake_generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.wake_ready.notify_all();
        generation
    }

    pub(super) fn wake_generation(&self) -> u64 {
        self.wake_generation.load(Ordering::Acquire)
    }

    pub(super) fn wait_for_wake_change(&self, observed_generation: u64, timeout: Duration) -> bool {
        if self.wake_generation() != observed_generation || self.should_stop() {
            return true;
        }
        if timeout.is_zero() {
            thread::yield_now();
            return self.wake_generation() != observed_generation || self.should_stop();
        }
        let guard = self
            .wake_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.wake_generation() != observed_generation || self.should_stop() {
            return true;
        }
        let (_guard, _) = self
            .wake_ready
            .wait_timeout_while(guard, timeout, |_| {
                self.wake_generation() == observed_generation && !self.should_stop()
            })
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.wake_generation() != observed_generation || self.should_stop()
    }
}

fn volume_to_storage(volume: f32) -> u32 {
    (normalize_playback_volume(volume) * PLAYBACK_VOLUME_SCALE as f32).round() as u32
}

#[derive(Clone)]
pub(super) struct FfmpegPlaybackInput {
    pub(super) session_id: PlaybackSessionId,
    pub(super) url: String,
    pub(super) http_headers: Vec<(String, String)>,
    pub(super) content_length: Option<u64>,
    pub(super) start_position_seconds: f64,
    pub(super) selected_tracks: crate::player::PlaybackTrackSelection,
    pub(super) cache_config: PlaybackCacheConfig,
}

pub(super) enum FfmpegCommand {
    Seek {
        session_id: PlaybackSessionId,
        position_seconds: f64,
        mode: PlaybackSeekMode,
        generation: u64,
        queued_at: Instant,
    },
    Pause {
        session_id: PlaybackSessionId,
    },
    Resume {
        session_id: PlaybackSessionId,
    },
    Stop,
    SetTrackSelection {
        session_id: PlaybackSessionId,
        selected_tracks: crate::player::PlaybackTrackSelection,
        position_seconds: f64,
        generation: u64,
        pause_after_switch: bool,
    },
    SetCacheConfig {
        session_id: PlaybackSessionId,
        config: PlaybackCacheConfig,
    },
    #[allow(dead_code)]
    SetPlaybackRate {
        session_id: PlaybackSessionId,
        rate: f64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlaybackCommandDisconnected;

fn send_playback_command(
    command_tx: &Sender<FfmpegCommand>,
    control: &FfmpegControl,
    command: FfmpegCommand,
) -> std::result::Result<(), PlaybackCommandDisconnected> {
    command_tx
        .send(command)
        .map_err(|_| PlaybackCommandDisconnected)?;
    // The queue publication must happen-before the generation change. A
    // waiter released by this wake can therefore always observe the command.
    control.wake();
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PendingSeek {
    pub(super) session_id: PlaybackSessionId,
    pub(super) position_seconds: f64,
    pub(super) mode: PlaybackSeekMode,
    pub(super) generation: u64,
    pub(super) queued_at: Instant,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PendingTrackSelection {
    pub(super) session_id: PlaybackSessionId,
    pub(super) selected_tracks: crate::player::PlaybackTrackSelection,
    pub(super) position_seconds: f64,
    pub(super) generation: u64,
    pub(super) pause_after_switch: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct DrainedFfmpegCommands {
    pub(super) pending_seek: Option<PendingSeek>,
    pub(super) pending_track_selection: Option<PendingTrackSelection>,
    pub(super) cache_config: Option<PlaybackCacheConfig>,
}

impl FfmpegWorker {
    pub(super) fn spawn(
        input: FfmpegPlaybackInput,
        video_output_queue: VideoOutputQueue,
        event_tx: Sender<BackendEvent>,
        volume: f32,
    ) -> Result<Self> {
        let session_id = input.session_id;
        let control = Arc::new(FfmpegControl::with_volume(session_id, volume));
        let (command_tx, command_rx) = mpsc::channel();
        let frame_presented = Arc::new(AtomicBool::new(false));
        let worker_control = Arc::clone(&control);
        let worker_presented = Arc::clone(&frame_presented);

        let handle = thread::Builder::new()
            .name("tiny-ffmpeg-backend".to_string())
            .spawn(move || {
                let result = isolate_playback_worker_unwind(|| {
                    super::playback_loop::run_ffmpeg_playback(
                        input,
                        video_output_queue,
                        event_tx.clone(),
                        worker_control.clone(),
                        command_rx,
                        worker_presented.clone(),
                    )
                });

                let result = match result {
                    PlaybackWorkerRunResult::Returned(result) => result,
                    PlaybackWorkerRunResult::Panicked(panic_message) => {
                        let event_session_id = worker_control.session_id();
                        tracing::error!(
                            session_id = ?event_session_id,
                            panic_message,
                            "FFmpeg playback worker caught unexpected unwind"
                        );
                        let kind = playback_worker_unwind_event_kind(
                            worker_presented.load(Ordering::Relaxed),
                            &panic_message,
                        );
                        let _ = event_tx.send(BackendEvent::new(event_session_id, kind));
                        worker_control.shutdown();
                        return;
                    }
                };

                if worker_control.should_stop() {
                    return;
                }

                let event_session_id = worker_control.session_id();
                match result {
                    Ok(()) => {
                        let _ = event_tx.send(BackendEvent::new(
                            event_session_id,
                            BackendEventKind::PlaybackEnded,
                        ));
                    }
                    Err(error) if worker_presented.load(Ordering::Relaxed) => {
                        tracing::error!(%error, "FFmpeg playback worker failed");
                        let _ = event_tx.send(BackendEvent::new(
                            event_session_id,
                            BackendEventKind::Fatal(error),
                        ));
                    }
                    Err(error) => {
                        tracing::error!(%error, "FFmpeg playback load failed");
                        let _ = event_tx.send(BackendEvent::new(
                            event_session_id,
                            BackendEventKind::LoadFailed(error),
                        ));
                    }
                }
            })
            .map_err(|error| BackendError::Ffmpeg(format!("创建 FFmpeg 解码线程失败：{error}")))?;

        Ok(Self {
            control,
            command_tx,
            handle,
        })
    }

    pub(super) fn seek(
        &self,
        position_seconds: f64,
        mode: PlaybackSeekMode,
        session_id: PlaybackSessionId,
    ) -> Result<()> {
        let generation = self.control.request_seek();
        self.control.set_cache_paused(false);
        tracing::debug!(
            ?session_id,
            position_seconds,
            ?mode,
            generation,
            "queueing FFmpeg seek command"
        );
        send_playback_command(
            &self.command_tx,
            &self.control,
            FfmpegCommand::Seek {
                session_id,
                position_seconds,
                mode,
                generation,
                queued_at: Instant::now(),
            },
        )
        .map_err(|_| {
            self.control.finish_seek(generation);
            self.control.finish_seek_audio_pause();
            BackendError::Ffmpeg("FFmpeg 解码线程已停止".to_string())
        })?;
        Ok(())
    }

    pub(super) fn set_paused(&self, paused: bool, session_id: PlaybackSessionId) -> Result<()> {
        self.control.set_user_paused(paused);
        let command = if paused {
            FfmpegCommand::Pause { session_id }
        } else {
            FfmpegCommand::Resume { session_id }
        };
        send_playback_command(&self.command_tx, &self.control, command)
            .map_err(|_| BackendError::Ffmpeg("FFmpeg 解码线程已停止".to_string()))?;
        Ok(())
    }

    pub(super) fn set_track_selection(
        &self,
        selected_tracks: crate::player::PlaybackTrackSelection,
        position_seconds: f64,
        session_id: PlaybackSessionId,
        pause_after_switch: bool,
    ) -> Result<()> {
        let generation = self.control.request_seek();
        self.control.set_cache_paused(false);
        tracing::debug!(
            ?session_id,
            position_seconds,
            generation,
            pause_after_switch,
            ?selected_tracks,
            "queueing FFmpeg track selection command"
        );
        send_playback_command(
            &self.command_tx,
            &self.control,
            FfmpegCommand::SetTrackSelection {
                session_id,
                selected_tracks,
                position_seconds,
                generation,
                pause_after_switch,
            },
        )
        .map_err(|_| {
            self.control.finish_seek(generation);
            self.control.finish_seek_audio_pause();
            BackendError::Ffmpeg("FFmpeg 解码线程已停止".to_string())
        })?;
        Ok(())
    }

    pub(super) fn set_volume(&self, volume: f32) {
        self.control.set_volume(volume);
    }

    pub(super) fn set_cache_config(
        &self,
        session_id: PlaybackSessionId,
        config: PlaybackCacheConfig,
    ) -> Result<()> {
        send_playback_command(
            &self.command_tx,
            &self.control,
            FfmpegCommand::SetCacheConfig {
                session_id,
                config: config.normalized(),
            },
        )
        .map_err(|_| BackendError::Ffmpeg("FFmpeg 解码线程已停止".to_string()))?;
        Ok(())
    }

    pub(super) fn is_paused(&self) -> bool {
        self.control.is_paused()
    }

    pub(super) fn stop(self) {
        let Self {
            control,
            command_tx,
            handle,
        } = self;
        control.shutdown();
        let _ = command_tx.send(FfmpegCommand::Stop);
        let _ = handle.join();
    }

    pub(super) fn stop_async(self) {
        let Self {
            control,
            command_tx,
            handle,
        } = self;
        control.shutdown();
        let _ = command_tx.send(FfmpegCommand::Stop);
        let _ = thread::Builder::new()
            .name("tiny-ffmpeg-stop".to_string())
            .spawn(move || {
                let _ = handle.join();
            });
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PlaybackWorkerRunResult {
    Returned(std::result::Result<(), String>),
    Panicked(String),
}

fn isolate_playback_worker_unwind(
    run: impl FnOnce() -> std::result::Result<(), String>,
) -> PlaybackWorkerRunResult {
    match catch_unwind(AssertUnwindSafe(run)) {
        Ok(result) => PlaybackWorkerRunResult::Returned(result),
        Err(payload) => {
            PlaybackWorkerRunResult::Panicked(panic_payload_message(payload.as_ref()).to_string())
        }
    }
}

fn playback_worker_unwind_event_kind(
    frame_presented: bool,
    panic_message: &str,
) -> BackendEventKind {
    if frame_presented {
        BackendEventKind::Fatal(format!("FFmpeg 播放线程异常终止：{panic_message}"))
    } else {
        BackendEventKind::LoadFailed(format!("FFmpeg 加载线程异常终止：{panic_message}"))
    }
}

fn panic_payload_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&'static str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic payload")
}

pub(super) fn drain_playback_commands(
    command_rx: &Receiver<FfmpegCommand>,
    control: &FfmpegControl,
) -> DrainedFfmpegCommands {
    let mut drained = DrainedFfmpegCommands::default();
    while let Ok(command) = command_rx.try_recv() {
        apply_playback_command(&mut drained, command, control);
    }
    drained
}

fn apply_playback_command(
    drained: &mut DrainedFfmpegCommands,
    command: FfmpegCommand,
    control: &FfmpegControl,
) {
    match command {
        FfmpegCommand::Seek {
            session_id,
            position_seconds,
            mode,
            generation,
            queued_at,
        } => {
            drained.pending_track_selection = None;
            drained.pending_seek = Some(PendingSeek {
                session_id,
                position_seconds: position_seconds.max(0.0),
                mode,
                generation,
                queued_at,
            });
        }
        FfmpegCommand::Pause { session_id } => {
            control.set_session_id(session_id);
            control.set_user_paused(true);
            if let Some(pending) = drained.pending_track_selection.as_mut() {
                pending.pause_after_switch = true;
            }
        }
        FfmpegCommand::Resume { session_id } => {
            control.set_session_id(session_id);
            control.set_user_paused(false);
            if let Some(pending) = drained.pending_track_selection.as_mut() {
                pending.pause_after_switch = false;
            }
        }
        FfmpegCommand::Stop => {
            control.shutdown();
        }
        FfmpegCommand::SetTrackSelection {
            session_id,
            selected_tracks,
            position_seconds,
            generation,
            pause_after_switch,
        } => {
            drained.pending_seek = None;
            drained.pending_track_selection = Some(PendingTrackSelection {
                session_id,
                selected_tracks,
                position_seconds: position_seconds.max(0.0),
                generation,
                pause_after_switch,
            });
        }
        FfmpegCommand::SetCacheConfig { session_id, config } => {
            control.set_session_id(session_id);
            drained.cache_config = Some(config.normalized());
        }
        FfmpegCommand::SetPlaybackRate { session_id, rate } => {
            control.set_session_id(session_id);
            tracing::debug!(
                rate,
                "FFmpeg playback-rate command queued but not implemented yet"
            );
        }
    }
}

pub(super) unsafe extern "C" fn ffmpeg_interrupt_callback(opaque: *mut c_void) -> c_int {
    if opaque.is_null() {
        return 0;
    }
    let control = unsafe { &*(opaque as *const FfmpegControl) };
    // A pending seek is handled by the coordinator and demux generations. Do
    // not abort format opening or a low-level FFmpeg call here: any slow read
    // result is fenced and discarded by its captured generation.
    control.should_stop() as c_int
}

#[cfg(test)]
mod unwind_tests {
    use super::{
        BackendEventKind, FfmpegCommand, FfmpegControl, PlaybackSeekMode, PlaybackSessionId,
        PlaybackWorkerRunResult, isolate_playback_worker_unwind, playback_worker_unwind_event_kind,
        send_playback_command,
    };
    use std::{
        sync::{Arc, mpsc},
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn playback_worker_unwind_is_converted_to_a_terminal_backend_event() {
        assert_eq!(
            isolate_playback_worker_unwind(|| panic!("initial audio invariant")),
            PlaybackWorkerRunResult::Panicked("initial audio invariant".to_string())
        );
        assert_eq!(
            isolate_playback_worker_unwind(|| Err("decode failure".to_string())),
            PlaybackWorkerRunResult::Returned(Err("decode failure".to_string()))
        );

        match playback_worker_unwind_event_kind(false, "before first frame") {
            BackendEventKind::LoadFailed(message) => {
                assert!(message.contains("before first frame"));
            }
            event => panic!("unexpected pre-presentation event: {event:?}"),
        }
        match playback_worker_unwind_event_kind(true, "after first frame") {
            BackendEventKind::Fatal(message) => {
                assert!(message.contains("after first frame"));
            }
            event => panic!("unexpected post-presentation event: {event:?}"),
        }
    }

    #[test]
    fn wake_generation_closes_the_notify_before_wait_race() {
        let control = FfmpegControl::new(PlaybackSessionId(1));
        let observed_generation = control.wake_generation();
        control.wake();

        assert!(control.wait_for_wake_change(observed_generation, Duration::from_secs(1)));
    }

    #[test]
    fn seek_interrupts_a_parked_playback_wait() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
        let observed_generation = control.wake_generation();
        let waiter_control = Arc::clone(&control);
        let waiter = thread::spawn(move || {
            let started_at = Instant::now();
            let changed =
                waiter_control.wait_for_wake_change(observed_generation, Duration::from_secs(1));
            (changed, started_at.elapsed())
        });

        let generation = control.request_seek();
        let (changed, elapsed) = waiter.join().expect("wake waiter joins");

        assert_eq!(generation, 1);
        assert!(changed);
        assert!(elapsed < Duration::from_millis(100), "elapsed={elapsed:?}");
    }

    #[test]
    fn seek_command_is_published_before_its_post_enqueue_wake() {
        let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
        let generation = control.request_seek();
        let observed_generation = control.wake_generation();
        let waiter_control = Arc::clone(&control);
        let (command_tx, command_rx) = mpsc::channel();
        let waiter = thread::spawn(move || {
            assert!(
                waiter_control.wait_for_wake_change(observed_generation, Duration::from_secs(1))
            );
            command_rx.try_recv()
        });

        send_playback_command(
            &command_tx,
            &control,
            FfmpegCommand::Seek {
                session_id: PlaybackSessionId(1),
                position_seconds: 42.0,
                mode: PlaybackSeekMode::Precise,
                generation,
                queued_at: Instant::now(),
            },
        )
        .expect("seek command remains connected");

        assert!(matches!(
            waiter.join().expect("wake waiter joins"),
            Ok(FfmpegCommand::Seek {
                generation: queued_generation,
                ..
            }) if queued_generation == generation
        ));
    }
}
