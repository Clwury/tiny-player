use super::*;

pub(super) struct FfmpegWorker {
    control: Arc<FfmpegControl>,
    command_tx: Sender<FfmpegCommand>,
    handle: JoinHandle<()>,
}

#[derive(Debug)]
pub(super) struct FfmpegControl {
    shutdown: AtomicBool,
    seek_generation: AtomicU64,
    handled_seek_generation: AtomicU64,
}

impl FfmpegControl {
    pub(super) fn new() -> Self {
        Self {
            shutdown: AtomicBool::new(false),
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

pub(super) struct FfmpegPlaybackInput {
    pub(super) url: String,
    pub(super) http_headers: Vec<(String, String)>,
    pub(super) content_length: Option<u64>,
    pub(super) start_position_seconds: f64,
}

pub(super) enum FfmpegCommand {
    Seek {
        position_seconds: f64,
        generation: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PendingSeek {
    pub(super) position_seconds: f64,
    pub(super) generation: u64,
}

impl FfmpegWorker {
    pub(super) fn spawn(
        input: FfmpegPlaybackInput,
        frame_slot: FrameSlot,
        event_tx: Sender<BackendEvent>,
    ) -> Result<Self> {
        let control = Arc::new(FfmpegControl::new());
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

                match result {
                    Ok(()) => {
                        let _ = event_tx.send(BackendEvent::Pause(true));
                    }
                    Err(error) if worker_presented.load(Ordering::Relaxed) => {
                        tracing::error!(%error, "FFmpeg playback worker failed");
                        let _ = event_tx.send(BackendEvent::Fatal(error));
                    }
                    Err(error) => {
                        tracing::error!(%error, "FFmpeg playback load failed");
                        let _ = event_tx.send(BackendEvent::LoadFailed(error));
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

    pub(super) fn seek(&self, position_seconds: f64) -> Result<()> {
        let generation = self.control.request_seek();
        self.command_tx
            .send(FfmpegCommand::Seek {
                position_seconds,
                generation,
            })
            .map_err(|_| {
                self.control.finish_seek(generation);
                BackendError::Ffmpeg("FFmpeg 解码线程已停止".to_string())
            })?;
        Ok(())
    }

    pub(super) fn stop(self) {
        let Self {
            control,
            command_tx: _,
            handle,
        } = self;
        control.shutdown();
        let _ = handle.join();
    }

    pub(super) fn stop_async(self) {
        let Self {
            control,
            command_tx: _,
            handle,
        } = self;
        control.shutdown();
        let _ = thread::Builder::new()
            .name("tiny-ffmpeg-stop".to_string())
            .spawn(move || {
                let _ = handle.join();
            });
    }
}

pub(super) fn drain_seek_command(command_rx: &Receiver<FfmpegCommand>) -> Option<PendingSeek> {
    let mut pending_seek = None;
    while let Ok(command) = command_rx.try_recv() {
        match command {
            FfmpegCommand::Seek {
                position_seconds,
                generation,
            } => {
                pending_seek = Some(PendingSeek {
                    position_seconds: position_seconds.max(0.0),
                    generation,
                });
            }
        }
    }
    pending_seek
}

pub(super) unsafe extern "C" fn ffmpeg_interrupt_callback(opaque: *mut c_void) -> c_int {
    if opaque.is_null() {
        return 0;
    }
    let control = unsafe { &*(opaque as *const FfmpegControl) };
    control.should_interrupt() as c_int
}
