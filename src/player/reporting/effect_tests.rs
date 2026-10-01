use super::*;
use crate::player::reporting::{PlaybackStopResult, ReportingIntent, test_support::Session};
use std::{sync::mpsc, time::Duration};

#[derive(Debug, PartialEq, Eq)]
enum Sent {
    Started,
    Progress(u64),
    Stopped,
}

#[derive(Default)]
struct FakeGateway {
    sent: Mutex<Vec<Sent>>,
    start_gate: Option<(mpsc::SyncSender<()>, Mutex<mpsc::Receiver<()>>)>,
    fail_stop: bool,
    panic_start: bool,
}

impl PlaybackReportGateway for FakeGateway {
    fn report(&self, report: &PlaybackReport) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push(match report {
            PlaybackReport::Started(_) => Sent::Started,
            PlaybackReport::Progress(report) => Sent::Progress(report.position_ticks),
            PlaybackReport::Stopped(_) => Sent::Stopped,
        });
        if matches!(report, PlaybackReport::Started(_)) {
            if let Some((entered, gate)) = &self.start_gate {
                entered.send(()).unwrap();
                gate.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
            assert!(!self.panic_start, "synthetic gateway panic");
        }
        anyhow::ensure!(
            !(self.fail_stop && matches!(report, PlaybackReport::Stopped(_))),
            "synthetic failure"
        );
        Ok(())
    }
}

fn worker(
    gateway: Arc<FakeGateway>,
    identity: WorkspaceIdentity,
) -> (PlaybackReporter, std::thread::JoinHandle<()>) {
    let mailbox = Arc::new(Mailbox::default());
    let worker = WorkerLifetime(mailbox.clone());
    let worker_identity = identity.clone();
    let handle = std::thread::spawn(move || run(worker, gateway.as_ref(), &worker_identity));
    (PlaybackReporter { mailbox, identity }, handle)
}

#[test]
fn pending_progress_is_bounded_and_only_latest_snapshot_is_sent() {
    let mut session = Session::new();
    let mailbox = Arc::new(Mailbox::default());
    mailbox.push(session.dispatch(ReportingIntent::Restart).command.unwrap());
    for position in 1..=10_000 {
        session.telemetry.position = Some(f64::from(position) / 1000.0);
        mailbox.push(
            session
                .dispatch(ReportingIntent::Progress { force: true })
                .command
                .unwrap(),
        );
    }
    {
        let pending = mailbox.pending.lock().unwrap();
        assert!(pending.started.is_some() && pending.progress.is_some());
        assert!(pending.stopped.is_none());
    }
    mailbox.close();
    let gateway = FakeGateway::default();
    run(WorkerLifetime(mailbox), &gateway, &session.identity);
    assert_eq!(
        *gateway.sent.lock().unwrap(),
        [Sent::Started, Sent::Progress(100_000_000)]
    );
}

#[test]
fn slow_start_then_owner_release_preserves_stop_and_discards_pending_progress() {
    for fail_stop in [false, true] {
        let mut session = Session::new();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let gateway = Arc::new(FakeGateway {
            start_gate: Some((entered_tx, Mutex::new(release_rx))),
            fail_stop,
            ..Default::default()
        });
        let (reporter, handle) = worker(gateway.clone(), session.identity.clone());
        reporter.send(session.dispatch(ReportingIntent::Restart).command.unwrap());
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        for position in 1..=1000 {
            session.telemetry.position = Some(f64::from(position) / 100.0);
            reporter.send(
                session
                    .dispatch(ReportingIntent::Progress { force: true })
                    .command
                    .unwrap(),
            );
        }
        let closed = session.dispatch(ReportingIntent::Close {
            failed: false,
            ended: false,
        });
        let receipt = closed.update.unwrap().stop_completion.unwrap();
        reporter.send(closed.command.unwrap());
        // A late command already created by another same-account session is
        // also rejected once this transport has accepted its terminal command.
        let mut late = Session::new();
        reporter.send(late.dispatch(ReportingIntent::Restart).command.unwrap());
        assert_eq!(receipt.result(), PlaybackStopResult::Pending);
        drop(session);
        drop(reporter);
        release_tx.send(()).unwrap();
        handle.join().unwrap();
        assert_eq!(
            *gateway.sent.lock().unwrap(),
            [Sent::Started, Sent::Stopped]
        );
        assert_eq!(
            receipt.result(),
            if fail_stop {
                PlaybackStopResult::Failed
            } else {
                PlaybackStopResult::Succeeded
            }
        );
    }
}

#[test]
fn foreign_account_commands_are_discarded_without_gateway_io() {
    let mut session = Session::new();
    let gateway = Arc::new(FakeGateway::default());
    let (reporter, handle) = worker(gateway.clone(), WorkspaceIdentity::default());
    reporter.send(session.dispatch(ReportingIntent::Restart).command.unwrap());
    let closed = session.dispatch(ReportingIntent::Close {
        failed: false,
        ended: false,
    });
    let receipt = closed.update.unwrap().stop_completion.unwrap();
    reporter.send(closed.command.unwrap());
    assert_eq!(receipt.result(), PlaybackStopResult::Failed);
    drop(reporter);
    handle.join().unwrap();
    assert!(gateway.sent.lock().unwrap().is_empty());
}

#[test]
fn worker_failure_resolves_pending_and_future_receipts_as_failed() {
    let mut session = Session::new();
    let mailbox = Arc::new(Mailbox::default());
    mailbox.push(session.dispatch(ReportingIntent::Restart).command.unwrap());
    let closed = session.dispatch(ReportingIntent::Close {
        failed: false,
        ended: false,
    });
    let receipt = closed.update.unwrap().stop_completion.unwrap();
    mailbox.push(closed.command.unwrap());
    let gateway = FakeGateway {
        panic_start: true,
        ..Default::default()
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(WorkerLifetime(mailbox.clone()), &gateway, &session.identity)
    }));
    assert!(result.is_err());
    assert_eq!(receipt.result(), PlaybackStopResult::Failed);
    let mut next = Session::new();
    next.dispatch(ReportingIntent::Restart);
    let closed = next.dispatch(ReportingIntent::Close {
        failed: false,
        ended: false,
    });
    let receipt = closed.update.unwrap().stop_completion.unwrap();
    mailbox.push(closed.command.unwrap());
    assert_eq!(receipt.result(), PlaybackStopResult::Failed);
    assert_eq!(*gateway.sent.lock().unwrap(), [Sent::Started]);
}

#[test]
fn queued_stop_keeps_start_first_even_before_worker_begins() {
    let mut session = Session::new();
    let mailbox = Arc::new(Mailbox::default());
    mailbox.push(session.dispatch(ReportingIntent::Restart).command.unwrap());
    mailbox.push(
        session
            .dispatch(ReportingIntent::Progress { force: true })
            .command
            .unwrap(),
    );
    let closed = session.dispatch(ReportingIntent::Close {
        failed: false,
        ended: true,
    });
    let receipt = closed.update.unwrap().stop_completion.unwrap();
    mailbox.push(closed.command.unwrap());
    let gateway = FakeGateway::default();
    run(WorkerLifetime(mailbox), &gateway, &session.identity);
    assert_eq!(
        *gateway.sent.lock().unwrap(),
        [Sent::Started, Sent::Stopped]
    );
    assert_eq!(receipt.result(), PlaybackStopResult::Succeeded);
}
