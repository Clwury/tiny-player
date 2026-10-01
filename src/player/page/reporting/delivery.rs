//! Stage accepted reports without locking the worker mailbox during render.
use std::{cell::RefCell, rc::Rc};

use crate::player::reporting::{
    ReportingCommand,
    effect::{Pending, PlaybackReporter},
};

/// Page-owned frontend; only foreground callbacks mutate staging. One detached
/// foreground continuation drains each batch after the current callback/frame.
/// It retains transport, not the page, so release still delivers an accepted
/// stop. Commands retain their original workspace/token and receipt; transport
/// rejection/failure remains silent apart from its log and completion result.
pub(super) struct ReportDelivery {
    state: Rc<RefCell<DeliveryState>>,
    executor: gpui::ForegroundExecutor,
}

struct DeliveryState {
    reporter: PlaybackReporter,
    pending: Pending,
    scheduled: bool,
}

impl ReportDelivery {
    pub(super) fn new(reporter: PlaybackReporter, executor: gpui::ForegroundExecutor) -> Self {
        Self {
            state: Rc::new(RefCell::new(DeliveryState {
                reporter,
                pending: Pending::default(),
                scheduled: false,
            })),
            executor,
        }
    }

    pub(super) fn send(&self, command: ReportingCommand) {
        let mut state = self.state.borrow_mut();
        if !state.reporter.accepts(&command) || !state.pending.push(command) || state.scheduled {
            return;
        }
        state.scheduled = true;
        drop(state);
        let state = self.state.clone();
        self.executor
            .spawn(async move { state.borrow_mut().flush() })
            .detach();
    }
}

#[cfg(test)]
mod tests;

impl DeliveryState {
    fn flush(&mut self) {
        self.scheduled = false;
        while let Some(command) = self.pending.take_next() {
            self.reporter.send(command);
        }
    }
}

impl Drop for DeliveryState {
    fn drop(&mut self) {
        // Executor teardown may drop the queued continuation instead of polling
        // it. Preserve accepted reports before closing the transport, just as
        // page release did before staging was introduced.
        self.flush();
    }
}
