use super::*;
use crate::player::{
    reporting::gateway::PlaybackReportGateway,
    reporting::{
        PlaybackReport, PlaybackStopCompletion, PlaybackStopResult, ReportingIntent,
        test_support::Session,
    },
};
use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

#[derive(Debug, PartialEq, Eq)]
enum Sent {
    Started,
    Progress(u64),
    Stopped,
}

#[derive(Debug)]
struct Gateway(mpsc::Sender<Sent>);

impl PlaybackReportGateway for Gateway {
    fn report(&self, report: &PlaybackReport) -> anyhow::Result<()> {
        self.0.send(match report {
            PlaybackReport::Started(_) => Sent::Started,
            PlaybackReport::Progress(report) => Sent::Progress(report.position_ticks),
            PlaybackReport::Stopped(_) => Sent::Stopped,
        })?;
        Ok(())
    }
}

fn reporter(session: &Session) -> (PlaybackReporter, mpsc::Receiver<Sent>) {
    let (tx, rx) = mpsc::channel();
    (
        PlaybackReporter::new(Arc::new(Gateway(tx)), session.identity.clone()),
        rx,
    )
}

fn receive(rx: &mpsc::Receiver<Sent>) -> Sent {
    rx.recv_timeout(Duration::from_secs(5)).unwrap()
}

fn assert_stopped(receipt: &PlaybackStopCompletion) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while receipt.result() == PlaybackStopResult::Pending {
        assert!(
            Instant::now() < deadline,
            "stop completion remained pending"
        );
        std::thread::yield_now();
    }
    assert_eq!(receipt.result(), PlaybackStopResult::Succeeded);
}

#[gpui::test]
fn render_submissions_wait_for_foreground_delivery_and_keep_only_latest_progress(
    cx: &mut gpui::TestAppContext,
) {
    let mut session = Session::new();
    let (reporter, rx) = reporter(&session);
    let delivery = ReportDelivery::new(reporter, cx.foreground_executor().clone());
    delivery.send(session.dispatch(ReportingIntent::Restart).command.unwrap());
    for position in 1..=10_000 {
        session.telemetry.position = Some(f64::from(position) / 1000.0);
        delivery.send(
            session
                .dispatch(ReportingIntent::Progress { force: true })
                .command
                .unwrap(),
        );
    }
    // No call has reached even the mailbox enqueue/lock, regardless of how
    // quickly its native worker runs. The current render callback can return.
    assert_eq!(delivery.state.borrow().reporter.enqueued_count(), 0);
    assert!(rx.try_recv().is_err());
    cx.run_until_parked();
    assert_eq!(delivery.state.borrow().reporter.enqueued_count(), 2);
    assert_eq!(receive(&rx), Sent::Started);
    assert_eq!(receive(&rx), Sent::Progress(100_000_000));
    drop(delivery);
}

#[gpui::test]
fn release_before_delivery_preserves_start_stop_and_rejects_foreign_close(
    cx: &mut gpui::TestAppContext,
) {
    let mut session = Session::new();
    let (reporter, rx) = reporter(&session);
    let delivery = ReportDelivery::new(reporter, cx.foreground_executor().clone());
    let mut foreign = Session::new();
    foreign.controller = crate::player::reporting::ReportingController::new(
        crate::effects::WorkspaceIdentity::default(),
    );
    foreign.dispatch(ReportingIntent::Restart);
    delivery.send(
        foreign
            .dispatch(ReportingIntent::Close {
                failed: false,
                ended: false,
            })
            .command
            .unwrap(),
    );
    delivery.send(session.dispatch(ReportingIntent::Restart).command.unwrap());
    delivery.send(
        session
            .dispatch(ReportingIntent::Progress { force: true })
            .command
            .unwrap(),
    );
    let closed = session.dispatch(ReportingIntent::Close {
        failed: false,
        ended: false,
    });
    let receipt = closed.update.unwrap().stop_completion.unwrap();
    delivery.send(closed.command.unwrap());
    let retired = Rc::downgrade(&delivery.state);
    drop(session);
    drop(delivery);
    assert!(
        retired.upgrade().is_some(),
        "accepted reports retain transport"
    );
    cx.run_until_parked();
    assert!(
        retired.upgrade().is_none(),
        "delivery releases its staging owner"
    );
    assert_eq!(receive(&rx), Sent::Started);
    assert_eq!(receive(&rx), Sent::Stopped);
    assert_stopped(&receipt);
    assert!(rx.try_recv().is_err());
}

#[test]
fn executor_teardown_flushes_accepted_stop_before_closing_transport() {
    let mut session = Session::new();
    let (reporter, rx) = reporter(&session);
    let mut pending = Pending::default();
    pending.push(session.dispatch(ReportingIntent::Restart).command.unwrap());
    let closed = session.dispatch(ReportingIntent::Close {
        failed: false,
        ended: false,
    });
    let receipt = closed.update.unwrap().stop_completion.unwrap();
    pending.push(closed.command.unwrap());
    // The last Rc disappears without polling its queued future at teardown.
    drop(DeliveryState {
        reporter,
        pending,
        scheduled: true,
    });
    assert_eq!(receive(&rx), Sent::Started);
    assert_eq!(receive(&rx), Sent::Stopped);
    assert_stopped(&receipt);
}
