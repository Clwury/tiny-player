use super::{Arc, AtomicBool, AtomicU64, Duration, Instant, JoinHandle, Ordering, thread};

const AUDIO_OUTPUT_SERVICE_STAGE_COUNT: usize = 6;
const AUDIO_OUTPUT_SERVICE_WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(5);
const AUDIO_OUTPUT_SERVICE_WATCHDOG_WARN_AFTER: Duration = Duration::from_millis(10);
const AUDIO_OUTPUT_SERVICE_WATCHDOG_LOG_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(in crate::player::backend::ffmpeg) enum AudioOutputServiceStage {
    StatusSnapshot = 0,
    StableSnapshot = 1,
    ResetClock = 2,
    StagePending = 3,
    PreparedSnapshot = 4,
    ControlCommit = 5,
}

impl AudioOutputServiceStage {
    const ALL: [Self; AUDIO_OUTPUT_SERVICE_STAGE_COUNT] = [
        Self::StatusSnapshot,
        Self::StableSnapshot,
        Self::ResetClock,
        Self::StagePending,
        Self::PreparedSnapshot,
        Self::ControlCommit,
    ];

    pub(in crate::player::backend::ffmpeg) fn as_str(self) -> &'static str {
        match self {
            Self::StatusSnapshot => "status_snapshot",
            Self::StableSnapshot => "stable_snapshot",
            Self::ResetClock => "reset_clock",
            Self::StagePending => "stage_pending",
            Self::PreparedSnapshot => "prepared_snapshot",
            Self::ControlCommit => "control_commit",
        }
    }
}

struct AudioOutputServiceStageState {
    next_sequence: AtomicU64,
    active_sequence: AtomicU64,
    started_at_nsecs: AtomicU64,
    last_elapsed_nsecs: AtomicU64,
    completed_count: AtomicU64,
}

impl AudioOutputServiceStageState {
    fn new() -> Self {
        Self {
            next_sequence: AtomicU64::new(0),
            active_sequence: AtomicU64::new(0),
            started_at_nsecs: AtomicU64::new(0),
            last_elapsed_nsecs: AtomicU64::new(0),
            completed_count: AtomicU64::new(0),
        }
    }
}

pub(in crate::player::backend::ffmpeg::audio) struct AudioOutputServiceTelemetry {
    origin: Instant,
    stages: [AudioOutputServiceStageState; AUDIO_OUTPUT_SERVICE_STAGE_COUNT],
    shutdown: AtomicBool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct AudioOutputServiceStageSnapshot {
    pub(in crate::player::backend::ffmpeg) stage: AudioOutputServiceStage,
    pub(in crate::player::backend::ffmpeg) active: bool,
    pub(in crate::player::backend::ffmpeg) active_elapsed_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) last_elapsed_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) started_count: u64,
    pub(in crate::player::backend::ffmpeg) completed_count: u64,
}

pub(in crate::player::backend::ffmpeg) struct AudioOutputServiceStageGuard {
    telemetry: Arc<AudioOutputServiceTelemetry>,
    stage: AudioOutputServiceStage,
    sequence: u64,
    started_at_nsecs: u64,
}

impl AudioOutputServiceTelemetry {
    pub(in crate::player::backend::ffmpeg::audio) fn new() -> Self {
        Self {
            origin: Instant::now(),
            stages: std::array::from_fn(|_| AudioOutputServiceStageState::new()),
            shutdown: AtomicBool::new(false),
        }
    }

    pub(in crate::player::backend::ffmpeg::audio) fn begin(
        self: &Arc<Self>,
        stage: AudioOutputServiceStage,
    ) -> AudioOutputServiceStageGuard {
        let state = &self.stages[stage as usize];
        let sequence = state
            .next_sequence
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1)
            .max(1);
        let started_at_nsecs = self.now_nsecs();
        state
            .started_at_nsecs
            .store(started_at_nsecs, Ordering::Release);
        state.active_sequence.store(sequence, Ordering::Release);
        AudioOutputServiceStageGuard {
            telemetry: Arc::clone(self),
            stage,
            sequence,
            started_at_nsecs,
        }
    }

    pub(in crate::player::backend::ffmpeg::audio) fn snapshots(
        &self,
    ) -> [AudioOutputServiceStageSnapshot; AUDIO_OUTPUT_SERVICE_STAGE_COUNT] {
        let now_nsecs = self.now_nsecs();
        std::array::from_fn(|index| {
            let stage = AudioOutputServiceStage::ALL[index];
            let state = &self.stages[index];
            let active_sequence = state.active_sequence.load(Ordering::Acquire);
            let started_at_nsecs = state.started_at_nsecs.load(Ordering::Acquire);
            AudioOutputServiceStageSnapshot {
                stage,
                active: active_sequence != 0,
                active_elapsed_nsecs: if active_sequence != 0 {
                    now_nsecs.saturating_sub(started_at_nsecs)
                } else {
                    0
                },
                last_elapsed_nsecs: state.last_elapsed_nsecs.load(Ordering::Acquire),
                started_count: state.next_sequence.load(Ordering::Acquire),
                completed_count: state.completed_count.load(Ordering::Acquire),
            }
        })
    }

    pub(in crate::player::backend::ffmpeg::audio) fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    fn now_nsecs(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_nanos())
            .unwrap_or(u64::MAX)
            .max(1)
    }
}

impl Drop for AudioOutputServiceStageGuard {
    fn drop(&mut self) {
        let state = &self.telemetry.stages[self.stage as usize];
        let elapsed_nsecs = self
            .telemetry
            .now_nsecs()
            .saturating_sub(self.started_at_nsecs);
        state
            .last_elapsed_nsecs
            .store(elapsed_nsecs, Ordering::Release);
        state.completed_count.fetch_add(1, Ordering::AcqRel);
        let _ = state.active_sequence.compare_exchange(
            self.sequence,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

pub(in crate::player::backend::ffmpeg::audio) fn spawn_audio_output_service_watchdog(
    telemetry: Arc<AudioOutputServiceTelemetry>,
) -> std::result::Result<JoinHandle<()>, String> {
    thread::Builder::new()
        .name("tiny-ffmpeg-ao-watchdog".to_string())
        .spawn(move || run_audio_output_service_watchdog(telemetry))
        .map_err(|error| format!("启动系统音频服务 watchdog 失败：{error}"))
}

fn run_audio_output_service_watchdog(telemetry: Arc<AudioOutputServiceTelemetry>) {
    let mut last_logged_at = [None; AUDIO_OUTPUT_SERVICE_STAGE_COUNT];
    while !telemetry.shutdown.load(Ordering::Acquire) {
        thread::sleep(AUDIO_OUTPUT_SERVICE_WATCHDOG_POLL_INTERVAL);
        let now = Instant::now();
        for snapshot in telemetry.snapshots() {
            if !snapshot.active
                || snapshot.active_elapsed_nsecs
                    < u64::try_from(AUDIO_OUTPUT_SERVICE_WATCHDOG_WARN_AFTER.as_nanos())
                        .unwrap_or(u64::MAX)
            {
                continue;
            }
            let index = snapshot.stage as usize;
            if last_logged_at[index].is_some_and(|last: Instant| {
                now.saturating_duration_since(last) < AUDIO_OUTPUT_SERVICE_WATCHDOG_LOG_INTERVAL
            }) {
                continue;
            }
            tracing::warn!(
                ao_service_stage = snapshot.stage.as_str(),
                stage_active = true,
                stage_elapsed_ms = snapshot.active_elapsed_nsecs as f64 / 1_000_000.0,
                stage_last_elapsed_ms = snapshot.last_elapsed_nsecs as f64 / 1_000_000.0,
                stage_started_count = snapshot.started_count,
                stage_completed_count = snapshot.completed_count,
                "independent AO service watchdog observed a stalled coordinator stage"
            );
            last_logged_at[index] = Some(now);
        }
    }
}
