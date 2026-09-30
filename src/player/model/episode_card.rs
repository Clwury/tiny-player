//! Borrowed identity and small display values for one visible episode row.
use crate::player::model::{progress::format_playback_time, time::EMBY_TICKS_PER_SECOND};
use crate::player::{PlaybackQueueItem, format_video_size, premiere_day};

pub(in crate::player) struct EpisodeCardVm<'a> {
    pub(in crate::player) item_id: &'a str,
    pub(in crate::player) image_tag: Option<&'a str>,
    pub(in crate::player) label: std::sync::Arc<str>,
    pub(in crate::player) metadata: Option<String>,
    pub(in crate::player) overview: Option<String>,
    pub(in crate::player) selected: bool,
}

impl<'a> EpisodeCardVm<'a> {
    pub(in crate::player) fn new(
        item: &'a PlaybackQueueItem,
        size: Option<u64>,
        selected: bool,
    ) -> Self {
        Self {
            item_id: &item.item_id,
            image_tag: item.primary_image_tag.as_deref(),
            label: item.episode_label.clone(),
            metadata: episode_metadata_label(item, size),
            overview: item
                .overview
                .as_deref()
                .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
                .filter(|text| !text.is_empty()),
            selected,
        }
    }
}

pub(in crate::player) fn episode_metadata_label(
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
