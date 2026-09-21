use crate::player::{PlaybackQueueItem, format_video_size, premiere_day};

use super::super::{format_playback_time, request::EMBY_TICKS_PER_SECOND};

pub(super) fn episode_metadata_label(
    item: &PlaybackQueueItem,
    size: Option<u64>,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(day) = item.premiere_date.as_deref().and_then(premiere_day) {
        parts.push(day.to_string());
    }
    if let Some(ticks) = item.run_time_ticks.filter(|ticks| *ticks > 0) {
        parts.push(format_playback_time(
            ticks as f64 / EMBY_TICKS_PER_SECOND as f64,
        ));
    }
    if let Some(size) = size.filter(|size| *size > 0) {
        parts.push(format_video_size(size));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}
