//! Presentation deadline ownership; no executor, window or IO dependencies.
use crate::effects::{RequestScope, RequestSlot, RequestToken, WorkspaceIdentity};

#[derive(Clone, Copy, Debug)]
pub(in crate::player) enum PresentationTimer {
    Controls,
    Volume,
    Rate,
    DownloadSpeed,
}

impl PresentationTimer {
    pub(in crate::player) const ALL: [Self; 4] = [
        Self::Controls,
        Self::Volume,
        Self::Rate,
        Self::DownloadSpeed,
    ];

    pub(in crate::player) fn index(self) -> usize {
        match self {
            Self::Controls => 0,
            Self::Volume => 1,
            Self::Rate => 2,
            Self::DownloadSpeed => 3,
        }
    }

    fn scope(self) -> RequestScope {
        match self {
            Self::Controls => RequestScope::PlaybackControlsHide,
            Self::Volume => RequestScope::PlaybackVolumeHide,
            Self::Rate => RequestScope::PlaybackRateHide,
            Self::DownloadSpeed => RequestScope::PlaybackSpeedRefresh,
        }
    }
}

// One page owns these slots. Input/render schedules deadlines; hover/reset
// cancels controls; back closes all slots permanently; release invalidates Drop.
pub(in crate::player) struct PresentationTimers {
    slots: [RequestSlot; 4],
    closed: bool,
}

impl PresentationTimers {
    pub(in crate::player) fn new(identity: WorkspaceIdentity) -> Self {
        Self {
            slots: PresentationTimer::ALL
                .map(|kind| RequestSlot::new(kind.scope(), identity.clone())),
            closed: false,
        }
    }

    pub(in crate::player) fn begin(&mut self, kind: PresentationTimer) -> Option<RequestToken> {
        let slot = &mut self.slots[kind.index()];
        if self.closed || (matches!(kind, PresentationTimer::DownloadSpeed) && slot.is_active()) {
            return None;
        }
        Some(slot.issue())
    }

    pub(in crate::player) fn complete(
        &mut self,
        kind: PresentationTimer,
        token: &RequestToken,
        identity: &WorkspaceIdentity,
    ) -> bool {
        !self.closed && token.is_for(identity) && self.slots[kind.index()].commit(token)
    }

    pub(in crate::player) fn cancel(&mut self, kind: PresentationTimer) {
        self.slots[kind.index()].invalidate();
    }

    pub(in crate::player) fn close(&mut self) {
        self.closed = true;
        for slot in &mut self.slots {
            slot.invalidate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_indicator_deadline_wins_and_other_scopes_are_independent() {
        let identity = WorkspaceIdentity::default();
        let mut timers = PresentationTimers::new(identity.clone());
        let speed = timers.begin(PresentationTimer::DownloadSpeed).unwrap();
        for kind in [
            PresentationTimer::Controls,
            PresentationTimer::Volume,
            PresentationTimer::Rate,
        ] {
            let first = timers.begin(kind).unwrap();
            let mut last = first.clone();
            for _ in 0..1000 {
                last = timers.begin(kind).unwrap();
            }
            assert!(!timers.complete(kind, &first, &identity));
            assert!(timers.complete(kind, &last, &identity));
            assert!(!timers.complete(kind, &last, &identity));
        }
        assert!(timers.complete(PresentationTimer::DownloadSpeed, &speed, &identity));
    }

    #[test]
    fn download_refresh_coalesces_until_first_deadline_without_postponing_it() {
        let identity = WorkspaceIdentity::default();
        let mut timers = PresentationTimers::new(identity.clone());
        let first = timers.begin(PresentationTimer::DownloadSpeed).unwrap();
        for _ in 0..1000 {
            assert!(timers.begin(PresentationTimer::DownloadSpeed).is_none());
        }
        assert!(timers.complete(PresentationTimer::DownloadSpeed, &first, &identity));
        let next = timers.begin(PresentationTimer::DownloadSpeed).unwrap();
        assert!(!timers.complete(PresentationTimer::DownloadSpeed, &first, &identity));
        assert!(timers.complete(PresentationTimer::DownloadSpeed, &next, &identity));
    }

    #[test]
    fn wrong_account_owner_or_scope_cannot_consume_a_deadline() {
        let identity = WorkspaceIdentity::default();
        let mut timers = PresentationTimers::new(identity.clone());
        let token = timers.begin(PresentationTimer::Volume).unwrap();
        let wrong_identity = WorkspaceIdentity {
            user_id: Some("other".into()),
            ..identity.clone()
        };
        assert!(!timers.complete(PresentationTimer::Volume, &token, &wrong_identity));
        let mut other = PresentationTimers::new(identity.clone());
        let other_token = other.begin(PresentationTimer::Volume).unwrap();
        assert!(!timers.complete(PresentationTimer::Volume, &other_token, &identity));
        let rate = timers.begin(PresentationTimer::Rate).unwrap();
        assert!(!timers.complete(PresentationTimer::Rate, &token, &identity));
        assert!(timers.complete(PresentationTimer::Volume, &token, &identity));
        assert!(timers.complete(PresentationTimer::Rate, &rate, &identity));
    }

    #[test]
    fn hover_cancel_and_return_invalidate_pending_deadlines() {
        let identity = WorkspaceIdentity::default();
        let mut timers = PresentationTimers::new(identity.clone());
        let controls = timers.begin(PresentationTimer::Controls).unwrap();
        let volume = timers.begin(PresentationTimer::Volume).unwrap();
        timers.cancel(PresentationTimer::Controls);
        assert!(!timers.complete(PresentationTimer::Controls, &controls, &identity));
        assert!(timers.complete(PresentationTimer::Volume, &volume, &identity));
        let tokens = PresentationTimer::ALL.map(|kind| timers.begin(kind).unwrap());
        timers.close();
        timers.close();
        for (kind, token) in PresentationTimer::ALL.into_iter().zip(tokens) {
            assert!(!timers.complete(kind, &token, &identity));
            assert!(timers.begin(kind).is_none());
        }
    }
}
