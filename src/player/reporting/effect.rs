//! Serial IO ownership outlives the page only to finish its accepted reports.
//! Pending progress is bounded to one snapshot; stop supersedes that snapshot.
use std::sync::{Arc, Condvar, Mutex};

use super::controller::{PlaybackReport, ReportingCommand};
use crate::{
    effects::WorkspaceIdentity, observability::TraceId,
    player::reporting::gateway::PlaybackReportGateway,
};

#[derive(Default)]
pub(in crate::player) struct Pending {
    started: Option<ReportingCommand>,
    progress: Option<ReportingCommand>,
    stopped: Option<ReportingCommand>,
    start_accepted: bool,
    closed: bool,
}

impl Pending {
    /// Shared by the foreground staging buffer and the worker mailbox. Each
    /// holds at most one start, one latest progress and one terminal report.
    pub(in crate::player) fn push(&mut self, command: ReportingCommand) -> bool {
        if self.closed {
            return false;
        }
        match &command.report {
            PlaybackReport::Started(_) if !self.start_accepted => {
                self.start_accepted = true;
                self.started = Some(command);
            }
            PlaybackReport::Progress(_) => self.progress = Some(command),
            PlaybackReport::Stopped(_) => {
                self.progress = None;
                self.stopped = Some(command);
                self.closed = true;
            }
            PlaybackReport::Started(_) => return false,
        }
        true
    }

    pub(in crate::player) fn take_next(&mut self) -> Option<ReportingCommand> {
        self.started
            .take()
            .or_else(|| self.stopped.take())
            .or_else(|| self.progress.take())
    }
}

#[derive(Default)]
struct Mailbox {
    pending: Mutex<Pending>,
    ready: Condvar,
    #[cfg(test)]
    enqueued: std::sync::atomic::AtomicUsize,
}

impl Mailbox {
    fn push(&self, command: ReportingCommand) {
        #[cfg(test)]
        self.enqueued
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut pending = self.pending.lock().unwrap();
        if pending.push(command) {
            self.ready.notify_one();
        }
    }

    fn next(&self) -> Option<ReportingCommand> {
        let mut pending = self.pending.lock().unwrap();
        loop {
            if let Some(command) = pending.take_next() {
                return Some(command);
            }
            if pending.closed {
                return None;
            }
            pending = self.ready.wait(pending).unwrap();
        }
    }

    fn close(&self) {
        self.pending.lock().unwrap().closed = true;
        self.ready.notify_one();
    }
}

// On an IO panic or failed spawn, drop receipts rather than leave them pending.
struct WorkerLifetime(Arc<Mailbox>);

impl Drop for WorkerLifetime {
    fn drop(&mut self) {
        let mut pending = self.0.pending.lock().unwrap();
        *pending = Pending {
            closed: true,
            ..Pending::default()
        };
        self.0.ready.notify_one();
    }
}

pub(in crate::player) struct PlaybackReporter {
    mailbox: Arc<Mailbox>,
    identity: WorkspaceIdentity,
}

impl PlaybackReporter {
    pub(in crate::player) fn new(
        gateway: Arc<dyn PlaybackReportGateway>,
        identity: WorkspaceIdentity,
    ) -> Self {
        let mailbox = Arc::new(Mailbox::default());
        let worker = WorkerLifetime(mailbox.clone());
        let worker_identity = identity.clone();
        if std::thread::Builder::new()
            .name("tiny-emby-playback-reporter".into())
            .spawn(move || run(worker, gateway.as_ref(), &worker_identity))
            .is_err()
        {
            mailbox.close();
            tracing::warn!("failed to spawn Emby playback reporter");
        }
        Self { mailbox, identity }
    }

    pub(in crate::player) fn send(&self, command: ReportingCommand) {
        if self.accepts(&command) {
            self.mailbox.push(command);
        }
    }

    pub(in crate::player) fn accepts(&self, command: &ReportingCommand) -> bool {
        command.accepts(&self.identity)
    }

    #[cfg(test)]
    pub(in crate::player) fn enqueued_count(&self) -> usize {
        self.mailbox
            .enqueued
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for PlaybackReporter {
    fn drop(&mut self) {
        self.mailbox.close();
    }
}

fn run(worker: WorkerLifetime, gateway: &dyn PlaybackReportGateway, identity: &WorkspaceIdentity) {
    while let Some(command) = worker.0.next() {
        if !command.accepts(identity) {
            continue;
        }
        let terminal = matches!(command.report, PlaybackReport::Stopped(_));
        let trace = TraceId::start(match command.report {
            PlaybackReport::Started(_) => "playback.report.start",
            PlaybackReport::Progress(_) => "playback.report.progress",
            PlaybackReport::Stopped(_) => "playback.report.stop",
        });
        let succeeded = gateway.report(&command.report).is_ok();
        trace.record(if succeeded { "committed" } else { "failed" });
        if !succeeded {
            tracing::warn!("Emby playback report failed");
        }
        command.finish(identity, succeeded);
        if terminal {
            break;
        }
    }
}

#[cfg(test)]
#[path = "effect_tests.rs"]
mod tests;
