use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

#[derive(Clone, Debug)]
pub struct PlaybackStateUpdate {
    pub item_id: String,
    // A grouped episode can have a different ID from the version being played.
    pub list_item_id: String,
    pub media_source_id: String,
    pub media_source_name: Option<String>,
    pub series_id: Option<String>,
    pub season_id: Option<String>,
    pub position_ticks: u64,
    pub run_time_ticks: Option<u64>,
    pub ended: bool,
    pub failed: bool,
    pub selected_item_id: Option<String>,
    pub stop_completion: Option<PlaybackStopCompletion>,
}

#[derive(Clone)]
pub struct PlaybackStopCompletion {
    state: Arc<AtomicU8>,
}

impl std::fmt::Debug for PlaybackStopCompletion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("PlaybackStopCompletion")
            .field(&self.result())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaybackStopResult {
    Pending,
    Succeeded,
    Failed,
}

impl PlaybackStopCompletion {
    pub(super) fn pending() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(0)),
        }
    }

    pub fn result(&self) -> PlaybackStopResult {
        match self.state.load(Ordering::Acquire) {
            1 => PlaybackStopResult::Succeeded,
            2 => PlaybackStopResult::Failed,
            _ => PlaybackStopResult::Pending,
        }
    }

    pub(super) fn finish(&self, succeeded: bool) {
        let _ = self.state.compare_exchange(
            0,
            if succeeded { 1 } else { 2 },
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}
