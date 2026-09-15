use super::*;
use crate::player::{backend::BackendEventKind, render_host::PlaybackSessionId};

use super::super::{
    FfmpegControl, PlaybackGeneration,
    decode::DecodePacketAdmissionStatus,
    subtitles::{SubtitleDecodeContext, SubtitlePipeline},
};

fn worker_with_channels() -> (
    SubtitleDecodeWorker,
    mpsc::SyncSender<SubtitleDecodeResult>,
    Receiver<SubtitleDecodeCommand>,
) {
    let (command_tx, command_rx) = mpsc::sync_channel(SUBTITLE_DECODE_COMMAND_QUEUE_CAPACITY);
    let (result_tx, result_rx) = mpsc::sync_channel(SUBTITLE_DECODE_RESULT_QUEUE_CAPACITY);
    (
        SubtitleDecodeWorker {
            command_tx,
            result_rx,
            handle: None,
            info: SubtitleDecodeWorkerInfo { stream_index: 2 },
            completed_packets: VecDeque::new(),
            in_flight_packets: 0,
            flush_generation: None,
            flush_command_sent: false,
            recovering: false,
        },
        result_tx,
        command_rx,
    )
}

fn subtitle_context() -> SubtitleDecodeContext {
    SubtitleDecodeContext {
        current_start_position_nsecs: 209_413_000_000,
        playback_timeline_origin_nsecs: Some(0),
    }
}

fn cue(text: &str) -> BackendSubtitleCue {
    BackendSubtitleCue {
        text: text.into(),
        bitmaps: Vec::new(),
        start_nsecs: 211_000_000_000,
        end_nsecs: 214_000_000_000,
    }
}

fn completed_packet(generation: u64, text: &str) -> SubtitleDecodeResult {
    SubtitleDecodeResult::PacketDone {
        generation,
        result: Ok(()),
        updates: vec![SubtitleCueUpdate::Push(cue(text))],
        elapsed: Duration::ZERO,
    }
}

#[test]
fn subtitle_output_drain_recovers_full_queue_after_seek_and_emits_new_cues() {
    let (worker, result_tx, command_rx) = worker_with_channels();
    let mut pipeline = SubtitlePipeline::with_worker_for_test(worker);
    let mut generation = PlaybackGeneration::default();
    let packet = AvPacket::new().unwrap();
    let session_id = PlaybackSessionId(34);
    let control = FfmpegControl::new(session_id);
    let (event_tx, event_rx) = mpsc::channel();

    for _ in 0..SUBTITLE_DECODE_COMMAND_QUEUE_CAPACITY {
        assert_eq!(
            pipeline
                .admit_demux_packet(&packet, &mut generation, subtitle_context(), session_id)
                .unwrap(),
            DecodePacketAdmissionStatus::Queued
        );
    }
    let recovery_generation = generation.advance();
    pipeline.flush_decode_state(recovery_generation).unwrap();
    assert_eq!(pipeline.front_generation(), None);
    let snapshot = pipeline.snapshot().unwrap();
    assert_eq!(snapshot.state, SubtitleDecodeWorkerState::Recovering);
    assert_eq!(snapshot.in_flight_packets, 4);
    assert!(SubtitlePipeline::block_reason_for(snapshot).is_some());

    // Let the worker finish the old commands. Flush could not be sent while
    // that queue was full, and there are no tracked generations left to poll.
    for _ in 0..SUBTITLE_DECODE_COMMAND_QUEUE_CAPACITY {
        let SubtitleDecodeCommand::Decode { generation, .. } = command_rx.try_recv().unwrap()
        else {
            panic!("expected pre-seek decode command");
        };
        result_tx
            .send(completed_packet(generation, "stale"))
            .unwrap();
    }
    pipeline
        .drain_ready_decode_output(None, &control, session_id, &event_tx)
        .unwrap();
    assert!(matches!(
        command_rx.try_recv(),
        Ok(SubtitleDecodeCommand::FlushBuffers { generation }) if generation == recovery_generation
    ));
    result_tx
        .send(SubtitleDecodeResult::Flushed {
            generation: recovery_generation,
        })
        .unwrap();
    pipeline
        .drain_ready_decode_output(None, &control, session_id, &event_tx)
        .unwrap();
    let snapshot = pipeline.snapshot().unwrap();
    assert_eq!(snapshot.state, SubtitleDecodeWorkerState::NeedPacket);
    assert_eq!(snapshot.in_flight_packets, 0);
    assert_eq!(snapshot.completed_packets, 0);
    pipeline.update_overlay(212_000_000_000, session_id, &event_tx);
    assert!(
        event_rx.try_recv().is_err(),
        "pre-seek cues must not reappear"
    );

    assert_eq!(
        pipeline
            .admit_demux_packet(&packet, &mut generation, subtitle_context(), session_id)
            .unwrap(),
        DecodePacketAdmissionStatus::Queued
    );
    let SubtitleDecodeCommand::Decode { generation, .. } = command_rx.try_recv().unwrap() else {
        panic!("expected new decode command after recovery");
    };
    result_tx
        .send(completed_packet(generation, "current"))
        .unwrap();
    assert!(
        pipeline
            .drain_ready_decode_output(None, &control, session_id, &event_tx)
            .unwrap()
    );
    pipeline.update_overlay(212_000_000_000, session_id, &event_tx);
    let event = event_rx
        .try_recv()
        .expect("subtitle becomes visible at 3:32");
    assert_eq!(event.session_id, session_id);
    assert!(
        matches!(event.kind, BackendEventKind::SubtitleChanged(Some(actual)) if actual == cue("current"))
    );
}

#[test]
fn consecutive_subtitle_seeks_wait_for_the_latest_flush_without_new_packets() {
    let (worker, result_tx, command_rx) = worker_with_channels();
    let mut pipeline = SubtitlePipeline::with_worker_for_test(worker);
    let session_id = PlaybackSessionId(34);
    let control = FfmpegControl::new(session_id);
    let (event_tx, event_rx) = mpsc::channel();

    pipeline.flush_decode_state(32).unwrap();
    pipeline.flush_decode_state(33).unwrap();
    for generation in [32, 33] {
        assert!(matches!(
            command_rx.try_recv(),
            Ok(SubtitleDecodeCommand::FlushBuffers { generation: actual }) if actual == generation
        ));
    }
    result_tx
        .send(SubtitleDecodeResult::Flushed { generation: 32 })
        .unwrap();
    result_tx.send(completed_packet(31, "stale")).unwrap();
    pipeline
        .drain_ready_decode_output(None, &control, session_id, &event_tx)
        .unwrap();
    assert_eq!(
        pipeline.snapshot().unwrap().state,
        SubtitleDecodeWorkerState::Recovering
    );

    result_tx
        .send(SubtitleDecodeResult::Flushed { generation: 33 })
        .unwrap();
    pipeline
        .drain_ready_decode_output(None, &control, session_id, &event_tx)
        .unwrap();
    assert_eq!(
        pipeline.snapshot().unwrap().state,
        SubtitleDecodeWorkerState::NeedPacket
    );
    pipeline.update_overlay(212_000_000_000, session_id, &event_tx);
    assert!(event_rx.try_recv().is_err());
}
