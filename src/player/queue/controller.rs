use super::model::*;
use crate::effects::{RequestScope, RequestSlot, RequestToken, WorkspaceIdentity};
use crate::player::model::queue::PlaybackQueue;

struct PendingSwitch {
    action: QueueAction,
    recovery: QueueFailureRecovery,
}

/// Page's sole queue owner. begin/complete/cancel are the mutation boundary.
/// Back, backend failure, and replacement/release invalidate its request slot.
pub(in crate::player) struct QueueController {
    queue: PlaybackQueue,
    request: RequestSlot,
    pending: Option<PendingSwitch>,
    error: Option<String>,
}

impl QueueController {
    pub(in crate::player) fn new(queue: PlaybackQueue, identity: WorkspaceIdentity) -> Self {
        Self {
            queue,
            request: RequestSlot::new(RequestScope::PlaybackQueue, identity),
            pending: None,
            error: None,
        }
    }
    pub(in crate::player) fn view_model(&self) -> QueueViewModel<'_> {
        QueueViewModel {
            queue: &self.queue,
            loading: self.pending.is_some(),
            error: self.error.as_deref(),
        }
    }
    pub(in crate::player) fn queue(&self) -> &PlaybackQueue {
        &self.queue
    }
    pub(in crate::player) fn begin(
        &mut self,
        action: QueueAction,
        automatic: bool,
        user_paused: bool,
        ended: bool,
    ) -> Option<QueueSwitchCommand> {
        if self.pending.is_some() {
            return None;
        }
        let target = match action {
            QueueAction::Previous => self.queue.previous_index(),
            QueueAction::Next => self.queue.next_index(),
            QueueAction::Select(index) => (index != self.queue.current_index).then_some(index),
        }?;
        self.queue.items.get(target)?;
        let recovery = QueueFailureRecovery {
            resume_current: !automatic && !user_paused && !ended,
            publish_terminal_update: automatic,
        };
        self.pending = Some(PendingSwitch { action, recovery });
        self.error = None;
        let mut queue = self.queue.clone();
        queue.current_index = target;
        Some(QueueSwitchCommand {
            token: self.request.issue(),
            queue,
            pause_current: recovery.resume_current,
        })
    }
    pub(in crate::player) fn cancel(&mut self) {
        self.request.invalidate();
        self.pending = None;
        self.error = None;
    }
    pub(in crate::player) fn pause_failed(
        &mut self,
        token: &RequestToken,
        identity: &WorkspaceIdentity,
        message: &str,
    ) -> bool {
        let Some(pending) = self.accept(token, identity) else {
            return false;
        };
        self.error = Some(format!("{}：{message}", pending.action.failure_prefix()));
        true
    }
    pub(in crate::player) fn complete(
        &mut self,
        command: QueueSwitchCommand,
        result: anyhow::Result<ResolvedQueuePlayback>,
        identity: &WorkspaceIdentity,
    ) -> Option<QueueSwitchUpdate> {
        let pending = self.accept(&command.token, identity)?;
        Some(match result {
            Ok(playback) => QueueSwitchUpdate::Replace(Box::new(QueueReplacement {
                playback,
                queue: command.queue,
                previous_index: self.queue.current_index,
            })),
            Err(error) => {
                self.error = Some(format!("{}：{error}", pending.action.failure_prefix()));
                QueueSwitchUpdate::Failed(pending.recovery)
            }
        })
    }
    fn accept(
        &mut self,
        token: &RequestToken,
        identity: &WorkspaceIdentity,
    ) -> Option<PendingSwitch> {
        if !token.is_for(identity) || !self.request.commit(token) {
            return None;
        }
        self.pending.take()
    }
    pub(in crate::player) fn resume_failed(&mut self, message: &str) {
        if let Some(error) = &mut self.error {
            error.push_str("；恢复当前播放失败：");
            error.push_str(message);
        }
    }
    #[cfg(test)]
    pub(in crate::player) fn queue_mut(&mut self) -> &mut PlaybackQueue {
        self.cancel();
        &mut self.queue
    }
}
