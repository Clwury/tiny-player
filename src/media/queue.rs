/// Playback request owns the queue until it is mounted in the session.
/// Index changes happen when resolving a switch; replacement drops the old queue.
#[derive(Clone)]
pub struct PlaybackQueueItem {
    pub item_id: String,
    pub title: std::sync::Arc<str>,
    pub episode_label: std::sync::Arc<str>,
    pub overview: Option<String>,
    pub primary_image_tag: Option<String>,
    pub series_id: Option<String>,
    pub season_id: Option<String>,
    pub premiere_date: Option<String>,
    pub run_time_ticks: Option<u64>,
    pub playback_position_ticks: Option<u64>,
    pub media_sources: Vec<crate::emby::MediaSource>,
}

impl std::fmt::Debug for PlaybackQueueItem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PlaybackQueueItem")
            .field("item_id", &self.item_id)
            .field("title", &self.title)
            .field("series_id", &self.series_id)
            .field("season_id", &self.season_id)
            .field("run_time_ticks", &self.run_time_ticks)
            .field("playback_position_ticks", &self.playback_position_ticks)
            .field("media_sources", &self.media_sources.len())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct PlaybackQueue {
    pub items: Vec<PlaybackQueueItem>,
    pub current_index: usize,
}

impl PlaybackQueue {
    pub fn new(items: Vec<PlaybackQueueItem>, current_index: usize) -> Self {
        let current_index = if items.is_empty() {
            0
        } else {
            current_index.min(items.len() - 1)
        };
        Self {
            items,
            current_index,
        }
    }

    pub fn current(&self) -> Option<&PlaybackQueueItem> {
        self.items.get(self.current_index)
    }

    pub fn previous_index(&self) -> Option<usize> {
        self.current_index.checked_sub(1)
    }

    pub fn next_index(&self) -> Option<usize> {
        let next = self.current_index.checked_add(1)?;
        (next < self.items.len()).then_some(next)
    }

    pub fn playlist_item_id(index: usize) -> String {
        format!("playlistItem{index}")
    }

    pub fn report_items(&self, playing_item_id: &str) -> Vec<crate::emby::PlaybackQueueReportItem> {
        self.items
            .iter()
            .enumerate()
            .map(|(index, item)| crate::emby::PlaybackQueueReportItem {
                id: if index == self.current_index {
                    playing_item_id.to_string()
                } else {
                    item.item_id.clone()
                },
                playlist_item_id: Self::playlist_item_id(index),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_queue_reports_stable_string_playlist_items() {
        let queue = PlaybackQueue::new(
            vec![
                queue_item("episode-1"),
                queue_item("episode-2"),
                queue_item("episode-3"),
            ],
            1,
        );

        assert_eq!(queue.previous_index(), Some(0));
        assert_eq!(queue.next_index(), Some(2));
        let reported_items = queue.report_items("episode-2-2160p");
        assert_eq!(reported_items[0].id, "episode-1");
        assert_eq!(reported_items[1].id, "episode-2-2160p");
        assert_eq!(reported_items[1].playlist_item_id, "playlistItem1");
        assert_eq!(queue.current().unwrap().item_id, "episode-2");
    }

    #[test]
    fn playback_queue_disables_out_of_bounds_neighbors() {
        let first = PlaybackQueue::new(vec![queue_item("episode-1")], 0);
        assert_eq!(first.previous_index(), None);
        assert_eq!(first.next_index(), None);

        let last = PlaybackQueue::new(vec![queue_item("episode-1"), queue_item("episode-2")], 1);
        assert_eq!(last.previous_index(), Some(0));
        assert_eq!(last.next_index(), None);
    }

    fn queue_item(item_id: &str) -> PlaybackQueueItem {
        PlaybackQueueItem {
            item_id: item_id.to_string(),
            title: item_id.to_string().into(),
            episode_label: item_id.to_string().into(),
            overview: None,
            primary_image_tag: None,
            series_id: Some("series-1".to_string()),
            season_id: Some("season-1".to_string()),
            premiere_date: None,
            run_time_ticks: None,
            playback_position_ticks: None,
            media_sources: Vec::new(),
        }
    }
}
