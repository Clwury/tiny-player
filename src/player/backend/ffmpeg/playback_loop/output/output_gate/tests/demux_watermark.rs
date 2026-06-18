use std::time::Duration;

use super::super::{
    DemuxReaderWatermark, OutputGateResumeTiming, timed_output_gate_demux_watermark,
};

#[test]
fn timed_output_gate_demux_watermark_returns_callback_watermark_and_tracks_time() {
    let expected = DemuxReaderWatermark {
        video_forward_nsecs: Some(123),
        audio_forward_nsecs: Some(456),
        selected_min_forward_nsecs: Some(123),
        forward_bytes: 789,
        ..DemuxReaderWatermark::default()
    };
    let mut calls = 0;
    let mut timing = OutputGateResumeTiming {
        demux_watermark: Duration::from_millis(7),
        ..OutputGateResumeTiming::default()
    };

    let watermark = timed_output_gate_demux_watermark(
        &mut || {
            calls += 1;
            expected
        },
        &mut timing,
    );

    assert_eq!(watermark, expected);
    assert_eq!(calls, 1);
    assert!(timing.demux_watermark >= Duration::from_millis(7));
}
