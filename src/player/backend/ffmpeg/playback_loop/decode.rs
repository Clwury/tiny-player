#[derive(Default)]
pub(super) struct PlaybackGeneration {
    next: u64,
}

impl PlaybackGeneration {
    pub(super) fn advance(&mut self) -> u64 {
        self.next = self.next.saturating_add(1);
        self.next
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DecodeInputRetryStatus {
    Idle,
    Queued,
    Backpressured,
}

impl DecodeInputRetryStatus {
    pub(super) fn made_progress(self) -> bool {
        matches!(self, Self::Queued)
    }

    pub(super) fn backpressured(self) -> bool {
        matches!(self, Self::Backpressured)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DecodePacketAdmissionStatus {
    Queued,
    Backpressured,
    Dropped,
}

impl DecodePacketAdmissionStatus {
    pub(super) fn backpressured(self) -> bool {
        matches!(self, Self::Backpressured)
    }
}
