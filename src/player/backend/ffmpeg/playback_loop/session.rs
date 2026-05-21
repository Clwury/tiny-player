use super::*;

pub(super) struct PlaybackSession {
    id: PlaybackSessionId,
    start_position_nsecs: u64,
}

impl PlaybackSession {
    pub(super) fn new(id: PlaybackSessionId, start_position_seconds: f64) -> Self {
        Self {
            id,
            start_position_nsecs: seconds_to_nsecs(start_position_seconds),
        }
    }

    pub(super) fn id(&self) -> PlaybackSessionId {
        self.id
    }

    pub(super) fn start_position_nsecs(&self) -> u64 {
        self.start_position_nsecs
    }

    pub(super) fn reset_to(&mut self, id: PlaybackSessionId, position_seconds: f64) {
        self.id = id;
        self.start_position_nsecs = seconds_to_nsecs(position_seconds.max(0.0));
    }
}
