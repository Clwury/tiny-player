use super::*;

pub(super) struct FfmpegWorker {
    control: Arc<FfmpegControl>,
    command_tx: Sender<FfmpegCommand>,
    handle: JoinHandle<()>,
}

#[derive(Debug)]
pub(super) struct FfmpegControl {
    shutdown: AtomicBool,
    paused: AtomicBool,
    volume: AtomicU32,
    session_id: AtomicU64,
    seek_generation: AtomicU64,
    handled_seek_generation: AtomicU64,
}

impl FfmpegControl {
    #[cfg(test)]
    pub(super) fn new(session_id: PlaybackSessionId) -> Self {
        Self::with_volume(session_id, DEFAULT_PLAYBACK_VOLUME)
    }

    pub(super) fn with_volume(session_id: PlaybackSessionId, volume: f32) -> Self {
        Self {
            shutdown: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            volume: AtomicU32::new(volume_to_storage(volume)),
            session_id: AtomicU64::new(session_id.0),
            seek_generation: AtomicU64::new(0),
            handled_seek_generation: AtomicU64::new(0),
        }
    }

    pub(super) fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    pub(super) fn should_stop(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
    }

    pub(super) fn should_interrupt(&self) -> bool {
        self.should_stop() || self.has_pending_seek()
    }

    pub(super) fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    pub(super) fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
    }

    pub(super) fn set_volume(&self, volume: f32) {
        self.volume
            .store(volume_to_storage(volume), Ordering::Release);
    }

    pub(super) fn volume(&self) -> f32 {
        self.volume.load(Ordering::Acquire) as f32 / PLAYBACK_VOLUME_SCALE as f32
    }

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
        self.seek_generation.fetch_add(1, Ordering::AcqRel) + 1
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
                Ok(_) => return,
                Err(next) => current = next,
            }
        }
    }

    pub(super) fn has_pending_seek(&self) -> bool {
        self.seek_generation.load(Ordering::Acquire)
            > self.handled_seek_generation.load(Ordering::Acquire)
    }
}

fn volume_to_storage(volume: f32) -> u32 {
    (normalize_playback_volume(volume) * PLAYBACK_VOLUME_SCALE as f32).round() as u32
}

pub(super) struct FfmpegPlaybackInput {
    pub(super) session_id: PlaybackSessionId,
    pub(super) url: String,
    pub(super) http_headers: Vec<(String, String)>,
    pub(super) content_length: Option<u64>,
    pub(super) start_position_seconds: f64,
    pub(super) selected_tracks: crate::player::PlaybackTrackSelection,
}

pub(super) enum FfmpegCommand {
    Seek {
        session_id: PlaybackSessionId,
        position_seconds: f64,
        generation: u64,
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
    #[allow(dead_code)]
    SetPlaybackRate {
        session_id: PlaybackSessionId,
        rate: f64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PendingSeek {
    pub(super) session_id: PlaybackSessionId,
    pub(super) position_seconds: f64,
    pub(super) generation: u64,
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
}

impl FfmpegWorker {
    pub(super) fn spawn(
        input: FfmpegPlaybackInput,
        frame_slot: FrameSlot,
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
                let result = super::playback_loop::run_ffmpeg_playback(
                    input,
                    frame_slot,
                    event_tx.clone(),
                    worker_control.clone(),
                    command_rx,
                    worker_presented.clone(),
                );

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

    pub(super) fn seek(&self, position_seconds: f64, session_id: PlaybackSessionId) -> Result<()> {
        let generation = self.control.request_seek();
        self.control.set_paused(false);
        self.command_tx
            .send(FfmpegCommand::Seek {
                session_id,
                position_seconds,
                generation,
            })
            .map_err(|_| {
                self.control.finish_seek(generation);
                BackendError::Ffmpeg("FFmpeg 解码线程已停止".to_string())
            })?;
        Ok(())
    }

    pub(super) fn set_paused(&self, paused: bool, session_id: PlaybackSessionId) -> Result<()> {
        self.control.set_paused(paused);
        let command = if paused {
            FfmpegCommand::Pause { session_id }
        } else {
            FfmpegCommand::Resume { session_id }
        };
        self.command_tx
            .send(command)
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
        self.control.set_paused(false);
        self.command_tx
            .send(FfmpegCommand::SetTrackSelection {
                session_id,
                selected_tracks,
                position_seconds,
                generation,
                pause_after_switch,
            })
            .map_err(|_| {
                self.control.finish_seek(generation);
                BackendError::Ffmpeg("FFmpeg 解码线程已停止".to_string())
            })?;
        Ok(())
    }

    pub(super) fn set_volume(&self, volume: f32) {
        self.control.set_volume(volume);
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

pub(super) fn drain_playback_commands(
    command_rx: &Receiver<FfmpegCommand>,
    control: &FfmpegControl,
) -> DrainedFfmpegCommands {
    let mut pending_seek = None;
    let mut pending_track_selection: Option<PendingTrackSelection> = None;
    while let Ok(command) = command_rx.try_recv() {
        match command {
            FfmpegCommand::Seek {
                session_id,
                position_seconds,
                generation,
            } => {
                pending_track_selection = None;
                pending_seek = Some(PendingSeek {
                    session_id,
                    position_seconds: position_seconds.max(0.0),
                    generation,
                });
            }
            FfmpegCommand::Pause { session_id } => {
                control.set_session_id(session_id);
                control.set_paused(true);
                if let Some(pending) = pending_track_selection.as_mut() {
                    pending.pause_after_switch = true;
                }
            }
            FfmpegCommand::Resume { session_id } => {
                control.set_session_id(session_id);
                control.set_paused(false);
                if let Some(pending) = pending_track_selection.as_mut() {
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
                pending_seek = None;
                pending_track_selection = Some(PendingTrackSelection {
                    session_id,
                    selected_tracks,
                    position_seconds: position_seconds.max(0.0),
                    generation,
                    pause_after_switch,
                });
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
    DrainedFfmpegCommands {
        pending_seek,
        pending_track_selection,
    }
}

pub(super) unsafe extern "C" fn ffmpeg_interrupt_callback(opaque: *mut c_void) -> c_int {
    if opaque.is_null() {
        return 0;
    }
    let control = unsafe { &*(opaque as *const FfmpegControl) };
    control.should_interrupt() as c_int
}
