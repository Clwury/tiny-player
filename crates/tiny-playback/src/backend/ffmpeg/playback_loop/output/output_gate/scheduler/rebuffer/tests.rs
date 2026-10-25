use std::sync::mpsc;

use super::*;
use crate::backend::BackendEventKind;

fn enter_underflow(output: &mut PlaybackOutputScheduler, control: &FfmpegControl) -> bool {
    output.maybe_enter_video_output_rebuffer(
        Instant::now(),
        true,
        Some(0),
        true,
        true,
        Some(0),
        false,
        0,
        true,
        false,
        control,
        None,
        None,
        control.session_id(),
        Some(0),
    )
}

#[test]
fn output_underflow_and_reset_report_once_without_entering_input_cache_pause() {
    let (tx, rx) = mpsc::channel();
    let session = PlaybackSessionId(1);
    let control = FfmpegControl::new(session).with_events(tx);
    let mut output = PlaybackOutputScheduler::new();
    output.set_state(PlaybackOutputState::Playing);

    assert!(enter_underflow(&mut output, &control));
    assert!(output.rebuffering());
    assert!(control.is_output_rebuffer_paused());
    assert!(!control.is_paused());
    let event = rx.try_recv().unwrap();
    assert_eq!(event.session_id, session);
    assert!(matches!(
        event.kind,
        BackendEventKind::OutputRebufferingChanged(true)
    ));
    assert!(!control.set_output_rebuffer_paused(true));
    assert!(rx.try_recv().is_err());

    output.clear_rebuffer(&control);
    let event = rx.try_recv().unwrap();
    assert_eq!(event.session_id, session);
    assert!(matches!(
        event.kind,
        BackendEventKind::OutputRebufferingChanged(false)
    ));
    output.clear_rebuffer(&control);
    assert!(rx.try_recv().is_err());
    assert!(!control.is_output_rebuffer_paused());
}

#[test]
fn seek_cancel_reports_cleared_output_wait_in_the_new_session() {
    let (tx, rx) = mpsc::channel();
    let control = FfmpegControl::new(PlaybackSessionId(1)).with_events(tx);
    let mut output = PlaybackOutputScheduler::new();
    output.set_state(PlaybackOutputState::Playing);
    assert!(enter_underflow(&mut output, &control));
    assert!(matches!(
        rx.try_recv().unwrap().kind,
        BackendEventKind::OutputRebufferingChanged(true)
    ));

    control.request_seek();
    assert!(!control.is_output_rebuffer_paused());
    control.set_session_id(PlaybackSessionId(2));
    let event = rx.try_recv().unwrap();
    assert_eq!(event.session_id, PlaybackSessionId(2));
    assert!(matches!(
        event.kind,
        BackendEventKind::OutputRebufferingChanged(false)
    ));
    output.clear_rebuffer(&control);
    control.set_session_id(PlaybackSessionId(2));
    assert!(rx.try_recv().is_err());
}
