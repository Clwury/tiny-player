use std::{
    sync::{Arc, atomic::AtomicBool, mpsc},
    time::{Duration, Instant},
};

use super::super::{
    AudioOutput, BufferedReporter, DecodedAudio, OutputGateResumeStatus, PlaybackOutputScheduler,
    PlaybackOutputState, PlaybackScheduler, PositionReporter, SubtitlePipeline,
    service_initial_video_clock_until_audio_start,
};
use super::test_queued_video_frame;
use crate::{
    BgraImage, VideoOutput, VideoPresenter,
    backend::ffmpeg::{AudioOutputDecision, FfmpegControl},
    render_host::{FramePixels, PlaybackSessionId, RenderSize, VideoOutputQueue},
};

const FRAME_NSECS: u64 = 40_000_000;

struct PausedAvSeek {
    control: Arc<FfmpegControl>,
    audio: AudioOutput,
    output: PlaybackOutputScheduler,
    clock: PlaybackScheduler,
    vo_queue: VideoOutputQueue,
    session_id: PlaybackSessionId,
    target: u64,
    audio_target: u64,
}

impl PausedAvSeek {
    fn new(target: u64, audio_delay_nsecs: u64) -> Self {
        let session_id = PlaybackSessionId(716);
        let control = Arc::new(FfmpegControl::new(session_id));
        control.set_user_paused(true);
        let generation = control.request_seek();
        control.finish_seek(generation);
        let audio = AudioOutput::stopped_for_test(Arc::clone(&control), 96_000, 48_000, 2);
        let audio_target = target + audio_delay_nsecs;
        audio.reset_clock(audio_target);
        let mut output = PlaybackOutputScheduler::new();
        for index in 0..8 {
            let mut frame = test_queued_video_frame(target + index * FRAME_NSECS);
            let gray = 30 + (target / 1_000_000_000 % 8 + index) as u8 * 10;
            frame.frame.pixels = FramePixels::Bgra8(vec![gray, gray, gray, 255].into());
            output.push_decoded_video_for_test(frame);
        }
        for index in 0..15 {
            let start = audio_target + index * 20_000_000;
            output.push_pending_start_audio_for_test(
                DecodedAudio {
                    samples: vec![0.25; 1920],
                    duration_nsecs: 20_000_000,
                },
                start,
                start + 20_000_000,
            );
        }
        output.begin_initial_av_start_transaction_for_generations(
            target,
            audio_target,
            generation,
            Instant::now() - Duration::from_nanos(audio_delay_nsecs) - Duration::from_millis(100),
        );
        let mut clock = PlaybackScheduler::new(target);
        clock.set_paused(true);
        clock.reset(target);
        let vo_queue = VideoOutputQueue::default();
        vo_queue.begin_session(session_id);
        Self {
            control,
            audio,
            output,
            clock,
            vo_queue,
            session_id,
            target,
            audio_target,
        }
    }

    fn service(&mut self) -> OutputGateResumeStatus {
        let (event_tx, _event_rx) = mpsc::channel();
        service_initial_video_clock_until_audio_start(
            &mut self.output,
            &self.audio,
            None,
            self.audio_target,
            Some(0),
            &self.control,
            self.session_id,
            &self.vo_queue,
            &AtomicBool::new(false),
            &mut PositionReporter::default(),
            &event_tx,
            &mut SubtitlePipeline::empty_for_test(),
            &mut BufferedReporter::new_with_events(true, false),
            &mut self.target,
            &mut self.clock,
        )
        .unwrap()
    }

    fn commit_preview(&mut self) {
        // Audio staging has a bounded per-tick budget; retry without advancing
        // the paused playback clock, even if the prepare spans several ticks.
        let mut status = self.service();
        for _ in 0..20 {
            if status == OutputGateResumeStatus::Resumed {
                break;
            }
            assert_eq!(status, OutputGateResumeStatus::Waiting);
            status = self.service();
        }
        assert_eq!(status, OutputGateResumeStatus::Resumed);
    }

    fn assert_preview_committed(&mut self) {
        self.commit_preview();
        assert_eq!(self.output.snapshot().state, PlaybackOutputState::Playing);
        assert!(self.control.is_user_paused());
        assert_eq!(self.clock.current_timeline_nsecs(), self.target);
        assert!(!self.clock.ready_for(self.target));
        assert_eq!(self.vo_queue.snapshot().queued_frames, 1);
        let preview = self.vo_queue.take_frame().unwrap();
        assert_eq!(preview.pts.unwrap().nsecs, self.target);
        assert_eq!(
            self.output.scheduled_video_queue.range_nsecs().unwrap().0,
            self.target + FRAME_NSECS
        );
        assert_eq!(
            self.control.audio_output_control_snapshot().decision(),
            AudioOutputDecision::Silence
        );
        let before = self.audio.snapshot().unwrap();
        while self.audio.transfer_next_queued_frame_for_test().unwrap() {}
        let mut callback = [1.0; 960];
        self.audio.invoke_callback_for_test(&mut callback);
        assert!(callback.iter().all(|sample| *sample == 0.0));
        let after = self.audio.snapshot().unwrap();
        assert_eq!(after.played_timeline_nsecs, before.played_timeline_nsecs);
        assert_eq!(after.total_pending_nsecs, before.total_pending_nsecs);
    }
}

#[test]
fn paused_av_seek_publishes_target_with_a_frozen_clock_and_silent_audio() {
    // These positions cover both seeks from the playback regression log and
    // a backward seek, without depending on a native audio device.
    for target in [6_967_294_000_000, 6_972_299_000_000, 1_200_000_000] {
        PausedAvSeek::new(target, 0).assert_preview_committed();
    }
}

#[test]
fn paused_av_seek_with_delayed_audio_publishes_only_the_target_preview() {
    PausedAvSeek::new(3_000_000_000, 3 * FRAME_NSECS).assert_preview_committed();
}

#[test]
fn newer_seek_interrupts_paused_av_preview_before_publication() {
    let mut seek = PausedAvSeek::new(3_000_000_000, 0);
    seek.control.request_seek();
    assert_eq!(seek.service(), OutputGateResumeStatus::Waiting);
    assert!(seek.vo_queue.take_frame().is_none());
    assert!(!seek.output.first_frame_presented);
    assert!(seek.control.is_user_paused());
}

fn render_preview(presenter: &mut VideoPresenter) -> BgraImage {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(image) = presenter
            .render_if_needed(RenderSize {
                width: 1,
                height: 1,
            })
            .unwrap()
        {
            return image;
        }
        assert!(
            Instant::now() < deadline,
            "paused seek preview did not reach the display"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn paused_av_seek_replaces_the_visible_image_without_resuming() {
    let vo_queue = VideoOutputQueue::default();
    let mut presenter = VideoPresenter::new(VideoOutput::new(vo_queue.clone())).unwrap();
    for (target, gray) in [
        (3_000_000_000, 60),
        (1_000_000_000, 40),
        (4_000_000_000, 70),
    ] {
        let mut seek = PausedAvSeek::new(target, 0);
        seek.vo_queue = vo_queue.clone();
        vo_queue.begin_session(seek.session_id);
        seek.commit_preview();
        let image = render_preview(&mut presenter);
        assert_eq!(image.bytes(), [gray, gray, gray, 255]);
        assert_eq!(
            vo_queue
                .snapshot()
                .last_presentation
                .unwrap()
                .timeline_nsecs,
            target
        );
        assert!(seek.control.is_user_paused());
        assert_eq!(seek.clock.current_timeline_nsecs(), target);
    }
}

#[test]
fn paused_av_seek_beyond_video_eof_publishes_last_frame_and_clears_loading() {
    use crate::backend::BackendEventKind;
    use crate::backend::ffmpeg::playback_loop::{
        video_frame_admission_service::{
            PreparedVideoFrameAdmissionContext, admit_paused_final_video_frame,
        },
        video_frame_prepare_worker::{DecodedVideoFrameDiagnostic, PreparedVideoFrame},
    };

    let mut seek = PausedAvSeek::new(6_400_000_000, 0);
    seek.output
        .reset_for_session(&seek.control, seek.session_id);
    let final_frame = test_queued_video_frame(5_900_000_000);
    let (event_tx, event_rx) = mpsc::channel();
    let admitted = admit_paused_final_video_frame(PreparedVideoFrameAdmissionContext {
        prepared_frame: PreparedVideoFrame {
            generation: 1,
            frame: final_frame.frame,
            timeline_nsecs: final_frame.timeline_nsecs,
            duration_nsecs: final_frame.duration_nsecs,
            source_frame_diagnostic: DecodedVideoFrameDiagnostic::default(),
        },
        decoded_video_frame_count: 8,
        scheduler: &mut seek.clock,
        audio_output: Some(&seek.audio),
        output_scheduler: &mut seek.output,
        buffered_reporter: &mut BufferedReporter::new_with_events(true, false),
        control: &seek.control,
        session_id: seek.session_id,
        event_tx: &event_tx,
        vo_queue: &seek.vo_queue,
        frame_presented: &AtomicBool::new(false),
        position_reporter: &mut PositionReporter::default(),
        subtitle_pipeline: &mut SubtitlePipeline::empty_for_test(),
        current_start_position_nsecs: &mut seek.target,
    });
    assert!(admitted);
    assert_eq!(
        seek.vo_queue.take_frame().unwrap().pts.unwrap().nsecs,
        5_900_000_000
    );
    assert!(
        event_rx
            .try_iter()
            .any(|event| matches!(event.kind, BackendEventKind::Buffering(false)))
    );
    assert_eq!(seek.output.snapshot().state, PlaybackOutputState::Playing);
    assert!(seek.control.is_user_paused());
    assert_eq!(seek.clock.current_timeline_nsecs(), 5_900_000_000);
    assert_eq!(
        seek.control.audio_output_control_snapshot().decision(),
        AudioOutputDecision::Silence
    );
    let mut callback = [1.0; 960];
    seek.audio.invoke_callback_for_test(&mut callback);
    assert!(callback.iter().all(|sample| *sample == 0.0));
}
