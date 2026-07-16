use std::time::Duration;

use gpui::{Context, Timer};

use crate::{
    emby::UserItemData,
    player::{PlaybackStateUpdate, PlaybackStopResult},
};

use super::{HomeContent, LoadState};

const PLAYBACK_REFRESH_POLL_INTERVAL: Duration = Duration::from_millis(250);
const PLAYBACK_REFRESH_POLL_LIMIT: usize = 160;

impl HomeContent {
    pub(super) fn apply_playback_update(
        &mut self,
        update: PlaybackStateUpdate,
        cx: &mut Context<Self>,
    ) {
        let previous = self.loaded_playback_user_data(&update.item_id).cloned();
        let user_data = playback_user_data_after_update(previous, &update);

        self.user_data_revision = self.user_data_revision.wrapping_add(1);
        self.user_data_item_revisions
            .insert(update.item_id.clone(), self.user_data_revision);
        self.user_data_overrides
            .insert(update.item_id.clone(), user_data.clone());

        if let Some(items) = self.resume_items.as_mut() {
            if update.ended {
                items.items.retain(|item| item.id != update.item_id);
            } else if let Some(item) = items
                .items
                .iter_mut()
                .find(|item| item.id == update.item_id)
            {
                item.user_data = Some(user_data.clone());
            }
        }
        if let Some(detail) = self.series_detail.as_mut() {
            detail.apply_playback_update(&update, &user_data);
        }

        self.schedule_home_snapshot_save(cx);
        self.schedule_playback_server_refresh(update.stop_completion.clone(), cx);
        cx.notify();
    }

    fn loaded_playback_user_data(&self, item_id: &str) -> Option<&UserItemData> {
        if let Some(data) = self.user_data_overrides.get(item_id) {
            return Some(data);
        }
        if let Some(data) = self
            .series_detail
            .as_ref()
            .and_then(|detail| detail.playback_user_data(item_id))
        {
            return Some(data);
        }
        if let Some(data) = self
            .resume_items
            .as_ref()
            .and_then(|items| items.items.iter().find(|item| item.id == item_id))
            .and_then(|item| item.user_data.as_ref())
        {
            return Some(data);
        }
        for row in self.user_view_items_rows.values() {
            if let Some(data) = row
                .items
                .as_ref()
                .and_then(|items| items.items.iter().find(|item| item.id == item_id))
                .and_then(|item| item.user_data.as_ref())
            {
                return Some(data);
            }
        }
        for library in self.libraries.values() {
            if let Some(data) = library
                .paged
                .items
                .iter()
                .find(|item| item.id == item_id)
                .and_then(|item| item.user_data.as_ref())
            {
                return Some(data);
            }
        }
        self.favorites
            .items
            .iter()
            .find(|item| item.id == item_id)
            .and_then(|item| item.user_data.as_ref())
            .or_else(|| {
                self.search
                    .items
                    .iter()
                    .find(|item| item.id == item_id)
                    .and_then(|item| item.user_data.as_ref())
            })
    }

    fn schedule_playback_server_refresh(
        &mut self,
        completion: Option<crate::player::PlaybackStopCompletion>,
        cx: &mut Context<Self>,
    ) {
        let Some(completion) = completion else {
            return;
        };
        self.playback_refresh_generation = self.playback_refresh_generation.wrapping_add(1);
        let generation = self.playback_refresh_generation;
        cx.spawn(async move |page, cx| {
            let mut result = completion.result();
            for _ in 0..PLAYBACK_REFRESH_POLL_LIMIT {
                if result != PlaybackStopResult::Pending {
                    break;
                }
                Timer::after(PLAYBACK_REFRESH_POLL_INTERVAL).await;
                result = completion.result();
            }
            page.update(cx, |page, cx| {
                if page.playback_refresh_generation != generation
                    || result != PlaybackStopResult::Succeeded
                {
                    return;
                }
                page.refresh_user_data_after_playback(cx);
            })
            .ok();
        })
        .detach();
    }

    fn refresh_user_data_after_playback(&mut self, cx: &mut Context<Self>) {
        if self.home_effects.resume_items != LoadState::Loading {
            self.home_effects.resume_items = LoadState::Idle;
            self.load_resume_items_if_needed(cx);
        }
        if let Some(detail) = self.series_detail.as_mut() {
            detail.mark_item_for_playback_refresh();
            detail.mark_episodes_for_playback_refresh();
        }
        self.load_series_media_item_if_needed(cx);
        self.load_series_episodes_if_needed(cx);
    }
}

fn playback_user_data_after_update(
    previous: Option<UserItemData>,
    update: &PlaybackStateUpdate,
) -> UserItemData {
    let mut data = previous.unwrap_or_default();
    if update.ended {
        data.playback_position_ticks = Some(0);
        data.played_percentage = Some(100.0);
        return data;
    }

    data.playback_position_ticks = Some(update.position_ticks);
    if let Some(runtime) = update.run_time_ticks.filter(|runtime| *runtime > 0) {
        data.played_percentage =
            Some((update.position_ticks as f64 / runtime as f64 * 100.0).clamp(0.0, 100.0));
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_playback_updates_position_and_preserves_favorite() {
        let update = playback_update(250, Some(1_000), false);
        let data = playback_user_data_after_update(
            Some(UserItemData {
                is_favorite: true,
                unplayed_item_count: Some(4),
                ..UserItemData::default()
            }),
            &update,
        );

        assert_eq!(data.playback_position_ticks, Some(250));
        assert_eq!(data.played_percentage, Some(25.0));
        assert!(data.is_favorite);
        assert_eq!(data.unplayed_item_count, Some(4));
    }

    #[test]
    fn ended_playback_clears_resume_position_and_marks_complete() {
        let update = playback_update(900, Some(1_000), true);
        let data = playback_user_data_after_update(None, &update);

        assert_eq!(data.playback_position_ticks, Some(0));
        assert_eq!(data.played_percentage, Some(100.0));
    }

    fn playback_update(
        position_ticks: u64,
        run_time_ticks: Option<u64>,
        ended: bool,
    ) -> PlaybackStateUpdate {
        PlaybackStateUpdate {
            item_id: "episode-1".to_string(),
            series_id: Some("series-1".to_string()),
            season_id: Some("season-1".to_string()),
            position_ticks,
            run_time_ticks,
            ended,
            failed: false,
            selected_item_id: None,
            stop_completion: None,
        }
    }
}
