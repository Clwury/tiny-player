use std::sync::mpsc::{self, Receiver};

use crate::player::{
    backend::BackendSubtitleCue,
    render_host::{FramePixels, FramePts, RenderSize},
};

use super::*;

const FRAME_DURATION_NSECS: u64 = 41_708_333;

struct VideoOnlyPlayback {
    scheduler: PlaybackScheduler,
    control: FfmpegControl,
    output_scheduler: PlaybackOutputScheduler,
    session_id: PlaybackSessionId,
    vo_queue: VideoOutputQueue,
    frame_presented: AtomicBool,
    position_reporter: PositionReporter,
    event_tx: Sender<BackendEvent>,
    event_rx: Receiver<BackendEvent>,
    subtitles: SubtitlePipeline,
    buffered_reporter: BufferedReporter,
    start_position_nsecs: u64,
    decoded_frames: u64,
}

impl VideoOnlyPlayback {
    fn new(start_position_nsecs: u64, cues: Vec<BackendSubtitleCue>) -> Self {
        let session_id = PlaybackSessionId(71);
        let vo_queue = VideoOutputQueue::default();
        vo_queue.begin_session(session_id);
        let (event_tx, event_rx) = mpsc::channel();
        Self {
            scheduler: PlaybackScheduler::new(start_position_nsecs),
            control: FfmpegControl::new(session_id),
            output_scheduler: PlaybackOutputScheduler::new(),
            session_id,
            vo_queue,
            frame_presented: AtomicBool::new(false),
            position_reporter: PositionReporter::default(),
            event_tx,
            event_rx,
            subtitles: SubtitlePipeline::with_external_cues_for_test(cues),
            buffered_reporter: BufferedReporter::new_with_events(false, true),
            start_position_nsecs,
            decoded_frames: 0,
        }
    }

    fn admit(&mut self, timeline_nsecs: u64) -> DecodedVideoAdmissionStatus {
        self.decoded_frames += 1;
        let status = service_video_clocked_decoded_video_frame(
            &mut self.scheduler,
            &self.control,
            &mut self.output_scheduler,
            self.session_id,
            &self.vo_queue,
            &self.frame_presented,
            &mut self.position_reporter,
            &self.event_tx,
            &mut self.subtitles,
            &mut self.buffered_reporter,
            DecodedFrame {
                size: RenderSize {
                    width: 1,
                    height: 1,
                },
                pts: Some(FramePts {
                    nsecs: timeline_nsecs,
                }),
                key_frame: true,
                pixels: FramePixels::Bgra8(vec![0, 0, 0, 255].into()),
            },
            timeline_nsecs,
            FRAME_DURATION_NSECS,
            &mut self.start_position_nsecs,
            self.decoded_frames,
        );
        // Hold queued frames until present_at explicitly advances the clock.
        self.scheduler.delay_by(Duration::from_secs(60));
        status
    }

    fn present_at(&mut self, timeline_nsecs: u64) {
        self.scheduler.reset(timeline_nsecs);
        assert!(service_video_clocked_video_queue(
            &self.scheduler,
            &self.control,
            &mut self.output_scheduler,
            self.session_id,
            &self.vo_queue,
            &self.frame_presented,
            &mut self.position_reporter,
            &self.event_tx,
            &mut self.subtitles,
            &mut self.buffered_reporter,
        ));
        self.scheduler.delay_by(Duration::from_secs(60));
    }

    fn seek(&mut self, timeline_nsecs: u64) {
        self.start_position_nsecs = timeline_nsecs;
        self.output_scheduler
            .reset_for_session(&self.control, self.session_id);
        self.subtitles.reset_cues_for_position(timeline_nsecs);
        self.vo_queue.begin_session(self.session_id);
    }

    fn subtitle_changes(&self) -> Vec<Option<String>> {
        self.event_rx
            .try_iter()
            .filter_map(|event| match event.kind {
                BackendEventKind::SubtitleChanged(cue) => Some(cue.map(|cue| cue.text)),
                _ => None,
            })
            .collect()
    }
}

fn cue(text: &str, start_nsecs: u64, end_nsecs: u64) -> BackendSubtitleCue {
    BackendSubtitleCue {
        text: text.into(),
        bitmaps: Vec::new(),
        start_nsecs,
        end_nsecs,
    }
}

#[test]
fn video_decode_ahead_does_not_flash_future_subtitles() {
    // The log alternated between the decoded 391.600s frame and the currently
    // presented 390.223s frame, repeatedly showing and hiding the same cue.
    let mut playback = VideoOnlyPlayback::new(
        390_223_000_000,
        vec![cue("next", 391_570_000_000, 392_770_000_000)],
    );
    assert_eq!(
        playback.admit(390_223_000_000),
        DecodedVideoAdmissionStatus::Stop
    );
    assert!(playback.subtitle_changes().is_empty());

    assert_eq!(
        playback.admit(391_600_000_000),
        DecodedVideoAdmissionStatus::Continue
    );
    assert!(
        playback.subtitle_changes().is_empty(),
        "decoding ahead must not show a future cue"
    );
    assert_eq!(playback.output_scheduler.scheduled_video_queue.len(), 1);

    playback.present_at(391_600_000_000);
    assert_eq!(playback.subtitle_changes(), vec![Some("next".into())]);
}

#[test]
fn video_decode_ahead_does_not_expire_current_subtitles() {
    let mut playback = VideoOnlyPlayback::new(
        388_930_000_000,
        vec![cue("current", 388_890_000_000, 391_130_000_000)],
    );
    playback.admit(388_930_000_000);
    assert_eq!(playback.subtitle_changes(), vec![Some("current".into())]);

    playback.admit(390_223_000_000);
    playback.admit(391_141_000_000);
    assert!(
        playback.subtitle_changes().is_empty(),
        "a queued frame past the cue end must not clear or evict it"
    );

    playback.present_at(390_223_000_000);
    assert!(
        playback.subtitle_changes().is_empty(),
        "the current cue must remain visible"
    );
    playback.present_at(391_141_000_000);
    assert_eq!(playback.subtitle_changes(), vec![None]);
}

#[test]
fn video_only_first_frame_restores_subtitles_after_forward_and_backward_seek() {
    let mut playback = VideoOnlyPlayback::new(
        388_430_000_000,
        vec![
            cue("first", 388_890_000_000, 391_130_000_000),
            cue("next", 391_570_000_000, 392_770_000_000),
        ],
    );
    playback.admit(388_930_000_000);
    assert_eq!(playback.start_position_nsecs, 388_930_000_000);
    assert_eq!(playback.subtitle_changes(), vec![Some("first".into())]);

    playback.seek(391_600_000_000);
    playback.admit(391_600_000_000);
    assert_eq!(playback.subtitle_changes(), vec![Some("next".into())]);

    playback.seek(388_930_000_000);
    playback.admit(388_930_000_000);
    assert_eq!(playback.subtitle_changes(), vec![Some("first".into())]);
}

#[test]
fn rejected_first_video_frame_does_not_publish_subtitles() {
    let mut playback = VideoOnlyPlayback::new(
        388_930_000_000,
        vec![cue("current", 388_890_000_000, 391_130_000_000)],
    );
    playback.vo_queue.begin_session(PlaybackSessionId(72));
    playback.admit(388_930_000_000);
    assert!(playback.subtitle_changes().is_empty());
    assert!(playback.output_scheduler.restart_pending());
}
