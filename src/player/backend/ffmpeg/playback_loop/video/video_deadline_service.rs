use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::player::{backend::BackendEvent, render_host::VideoOutputQueue};

use super::scheduled_video_queue::ScheduledVideoDeadlineQueue;
use super::video_output_gate::admit_decoded_video_frame_to_vo;
use super::{AudioClockHandle, PositionReporter};

const VIDEO_DEADLINE_POLL_INTERVAL: Duration = Duration::from_millis(5);
const VIDEO_DEADLINE_IDLE_WAIT: Duration = Duration::from_millis(20);
pub(in crate::player::backend::ffmpeg) const VIDEO_MINIMUM_PRESENT_INTERVAL: Duration =
    Duration::from_millis(100);

struct VideoDeadlineServiceControl {
    shutdown: AtomicBool,
    audio_clock: Mutex<Option<AudioClockHandle>>,
}

pub(in crate::player::backend::ffmpeg) struct VideoDeadlineService {
    control: Arc<VideoDeadlineServiceControl>,
    queue: ScheduledVideoDeadlineQueue,
    worker: Option<JoinHandle<()>>,
}

impl VideoDeadlineService {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::player::backend::ffmpeg) fn spawn(
        queue: ScheduledVideoDeadlineQueue,
        audio_clock: Option<AudioClockHandle>,
        vo_queue: VideoOutputQueue,
        frame_presented: Arc<AtomicBool>,
        event_tx: Sender<BackendEvent>,
    ) -> std::result::Result<Self, String> {
        let control = Arc::new(VideoDeadlineServiceControl {
            shutdown: AtomicBool::new(false),
            audio_clock: Mutex::new(audio_clock),
        });
        let worker_control = Arc::clone(&control);
        let worker_queue = queue.clone();
        let worker = match thread::Builder::new()
            .name("tiny-video-deadline".to_string())
            .spawn(move || {
                run_video_deadline_service(
                    worker_control,
                    worker_queue,
                    vo_queue,
                    frame_presented,
                    event_tx,
                )
            }) {
            Ok(worker) => worker,
            Err(error) => {
                queue.detach();
                return Err(format!("启动视频截止时间服务失败：{error}"));
            }
        };
        Ok(Self {
            control,
            queue,
            worker: Some(worker),
        })
    }

    pub(in crate::player::backend::ffmpeg) fn update_audio_clock(
        &self,
        audio_clock: Option<AudioClockHandle>,
    ) {
        *self
            .control
            .audio_clock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = audio_clock;
        self.queue.wake();
    }
}

impl Drop for VideoDeadlineService {
    fn drop(&mut self) {
        self.control.shutdown.store(true, Ordering::Release);
        self.queue.wake();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::warn!("video deadline service thread panicked during shutdown");
        }
        self.queue.detach();
    }
}

fn run_video_deadline_service(
    control: Arc<VideoDeadlineServiceControl>,
    queue: ScheduledVideoDeadlineQueue,
    vo_queue: VideoOutputQueue,
    frame_presented: Arc<AtomicBool>,
    event_tx: Sender<BackendEvent>,
) {
    let mut position_reporter = PositionReporter::default();
    let mut last_actual_presentation_at = None::<Instant>;
    while !control.shutdown.load(Ordering::Acquire) {
        if !queue.active() {
            queue.wait_for_change(VIDEO_DEADLINE_IDLE_WAIT);
            continue;
        }
        let deadline_session_id = queue.presentation_session_id();
        let active_vo_session_id = vo_queue.presentation_identity().0;
        if deadline_session_id != active_vo_session_id {
            if queue.suspend_for_session_mismatch(deadline_session_id) {
                tracing::error!(
                    deadline_session_id = ?deadline_session_id,
                    active_vo_session_id = ?active_vo_session_id,
                    "suspended video deadline service after playback session mismatch"
                );
            }
            continue;
        }
        let audio_clock = control
            .audio_clock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(played_until_nsecs) = audio_clock.and_then(|clock| clock.played_timeline_nsecs())
        else {
            queue.wait_for_change(VIDEO_DEADLINE_IDLE_WAIT);
            continue;
        };

        let force_latest_due =
            video_minimum_frame_rate_force_due(last_actual_presentation_at, Instant::now());
        let (pending, dropped_frames, forced_for_minimum_frame_rate) =
            queue.pop_audio_clocked_frame(played_until_nsecs, force_latest_due);
        if dropped_frames > 0 {
            let session_id = queue.presentation_session_id();
            tracing::debug!(
                session_id = ?session_id,
                dropped_video_frames = dropped_frames,
                scheduler_dropped_video_frames = queue.scheduler_dropped_video_frames(),
                played_until_nsecs,
                queued_video_frames = queue.len(),
                forced_for_minimum_frame_rate,
                "video deadline service dropped superseded audio-clocked frames"
            );
        }
        if let Some(pending) = pending {
            let admitted = queue.admit_if_current(pending, |queued, session_id| {
                let timeline_nsecs = queued.timeline_nsecs;
                admit_decoded_video_frame_to_vo(
                    queued.frame,
                    session_id,
                    timeline_nsecs,
                    &vo_queue,
                    &frame_presented,
                    &mut position_reporter,
                    &event_tx,
                )
            });
            if admitted == Some(true) {
                last_actual_presentation_at = Some(Instant::now());
                if forced_for_minimum_frame_rate {
                    let session_id = queue.presentation_session_id();
                    tracing::debug!(
                        session_id = ?session_id,
                        played_until_nsecs,
                        minimum_present_interval_ms =
                            VIDEO_MINIMUM_PRESENT_INTERVAL.as_millis(),
                        "video deadline service forced latest due frame to prevent freeze"
                    );
                }
            }
            continue;
        }

        let wait = queue
            .audio_clock_wait_duration(played_until_nsecs)
            .unwrap_or(VIDEO_DEADLINE_IDLE_WAIT)
            .min(VIDEO_DEADLINE_POLL_INTERVAL);
        queue.wait_for_change(wait.max(Duration::from_millis(1)));
    }
}

fn video_minimum_frame_rate_force_due(
    last_actual_presentation_at: Option<Instant>,
    now: Instant,
) -> bool {
    last_actual_presentation_at
        .is_none_or(|last| now.saturating_duration_since(last) >= VIDEO_MINIMUM_PRESENT_INTERVAL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimum_frame_rate_force_arms_at_one_hundred_milliseconds() {
        let now = Instant::now();

        assert!(video_minimum_frame_rate_force_due(None, now));
        assert!(!video_minimum_frame_rate_force_due(
            Some(now - Duration::from_millis(99)),
            now,
        ));
        assert!(video_minimum_frame_rate_force_due(
            Some(now - Duration::from_millis(100)),
            now,
        ));
    }
}
