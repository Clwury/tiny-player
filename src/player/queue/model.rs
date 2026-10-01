use crate::media::gateway::ResolvedPlayback;
use crate::{
    effects::RequestToken,
    player::{PlaybackQueue, PlaybackTrackPreferenceKey, reporting::PlaybackStateUpdate},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player) enum QueueAction {
    Previous,
    Next,
    Select(usize),
}
impl QueueAction {
    pub(super) fn failure_prefix(self) -> &'static str {
        match self {
            Self::Previous => "切换上一集失败",
            Self::Next => "切换下一集失败",
            Self::Select(_) => "切换剧集失败",
        }
    }
}

/// Immutable queue snapshot handed to IO. Accepted only by its creating owner.
#[derive(Clone)]
pub(in crate::player) struct QueueSwitchCommand {
    pub(in crate::player) token: RequestToken,
    pub(in crate::player) queue: PlaybackQueue,
    pub(in crate::player) pause_current: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player) struct QueueFailureRecovery {
    pub(in crate::player) resume_current: bool,
    pub(in crate::player) publish_terminal_update: bool,
}

pub(in crate::player) struct QueueViewModel<'a> {
    pub(in crate::player) queue: &'a PlaybackQueue,
    pub(in crate::player) loading: bool,
    pub(in crate::player) error: Option<&'a str>,
}
impl QueueViewModel<'_> {
    pub(in crate::player) fn has_episode_list(&self) -> bool {
        self.queue.current().is_some_and(|item| {
            item.series_id
                .as_deref()
                .is_some_and(|id| !id.trim().is_empty())
        })
    }

    pub(in crate::player) fn can_previous(&self) -> bool {
        !self.loading && self.queue.previous_index().is_some()
    }
    pub(in crate::player) fn can_next(&self) -> bool {
        !self.loading && self.queue.next_index().is_some()
    }
}

pub(in crate::player) struct QueueReplacement {
    pub(in crate::player) playback: ResolvedQueuePlayback,
    pub(in crate::player) queue: PlaybackQueue,
    pub(super) previous_index: usize,
}
impl QueueReplacement {
    pub(in crate::player) fn apply_close(&mut self, update: &mut PlaybackStateUpdate) {
        if let Some(previous) = self.queue.items.get_mut(self.previous_index) {
            previous.playback_position_ticks = Some(if update.ended {
                0
            } else {
                update.position_ticks
            });
        }
        update.selected_item_id = self.queue.current().map(|item| item.item_id.clone());
    }
}

pub(in crate::player) enum QueueSwitchUpdate {
    Replace(Box<QueueReplacement>),
    Failed(QueueFailureRecovery),
}

pub(in crate::player) struct ResolvedQueuePlayback {
    pub(in crate::player) source: ResolvedPlayback,
    pub(in crate::player) title: std::sync::Arc<str>,
    pub(in crate::player) audio_tracks: Vec<tiny_playback::PlaybackTrack>,
    pub(in crate::player) subtitle_tracks: Vec<tiny_playback::PlaybackTrack>,
    pub(in crate::player) selected_tracks: tiny_playback::PlaybackTrackSelection,
    pub(in crate::player) track_preference_key: PlaybackTrackPreferenceKey,
    pub(in crate::player) initial_position_seconds: f64,
    pub(in crate::player) run_time_ticks: Option<u64>,
}
