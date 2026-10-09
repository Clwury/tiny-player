use super::HomeController;
use crate::home::played::{PlayedCompletion, PlayedContent, PlayedUpdate};
use crate::{
    effects::{RequestToken, WorkspaceIdentity},
    emby::UserItemData,
    home::detail::controller::{DetailId, DetailUpdate},
    player::{PlaybackStateUpdate, PlaybackStopResult},
};

impl HomeController {
    pub(in crate::home) fn apply_played_completion(
        &mut self,
        completion: PlayedCompletion,
    ) -> PlayedUpdate {
        let detail_activation = self
            .navigation
            .detail()
            .and_then(|detail| detail.activation().cloned());
        let (current, history) = self.navigation.detail_models_mut();
        let content = PlayedContent {
            current,
            history: history.collect(),
            items: self
                .favorites
                .items()
                .chain(self.search.view_model().items.iter())
                .chain(
                    self.libraries
                        .values()
                        .chain(self.genres.values())
                        .flat_map(|library| &library.view_model().paged.items),
                )
                .chain(
                    self.persons
                        .values()
                        .flat_map(|person| &person.items.view_model().paged.items),
                )
                .chain(
                    self.feed
                        .state
                        .user_view_items_rows
                        .values()
                        .flat_map(|row| row.items.iter().flat_map(|items| &items.items)),
                )
                .collect(),
            resume: &mut self.feed.state.resume_items,
            detail_activation,
        };
        completion.apply(content, &mut self.user_data)
    }

    pub(in crate::home) fn apply_playback_update(
        &mut self,
        update: &PlaybackStateUpdate,
    ) -> Option<(DetailId, DetailUpdate)> {
        let previous = self
            .loaded_playback_user_data(&update.item_id)
            .or_else(|| self.loaded_playback_user_data(&update.list_item_id))
            .cloned();
        let user_data = playback_user_data_after_update(previous, update);

        if !update.failed && update.position_ticks > 0 && !update.media_source_id.is_empty() {
            for item_id in [&update.item_id, &update.list_item_id] {
                let version = self
                    .played_video_versions
                    .entry(item_id.clone())
                    .or_default();
                version.source_id = update.media_source_id.clone();
                version.name = update.media_source_name.clone();
            }
        }

        self.user_data.revision = self.user_data.revision.wrapping_add(1);
        for item_id in [&update.item_id, &update.list_item_id] {
            self.user_data
                .item_revisions
                .insert(item_id.clone(), self.user_data.revision);
            self.user_data
                .overrides
                .insert(item_id.clone(), user_data.clone());
        }

        if let Some(items) = self.feed.state.resume_items.as_mut() {
            if update.ended {
                items
                    .items
                    .retain(|item| item.id != update.item_id && item.id != update.list_item_id);
            } else {
                for item in items
                    .items
                    .iter_mut()
                    .filter(|item| item.id == update.item_id || item.id == update.list_item_id)
                {
                    item.user_data = Some(user_data.clone());
                }
            }
        }
        self.navigation.detail_mut().map(|detail| {
            (
                detail.id(),
                detail.apply_playback_update(update, &user_data),
            )
        })
    }

    pub(in crate::home) fn begin_playback_refresh(&mut self) -> RequestToken {
        self.playback_refresh.issue()
    }

    pub(in crate::home) fn complete_playback_refresh(
        &mut self,
        token: &RequestToken,
        identity: &WorkspaceIdentity,
        result: PlaybackStopResult,
    ) -> bool {
        if !token.is_for(identity)
            || !self.playback_refresh.commit(token)
            || result != PlaybackStopResult::Succeeded
        {
            return false;
        }
        if let Some(detail) = self.navigation.detail_mut() {
            detail.state.mark_item_for_playback_refresh();
            detail.state.mark_episodes_for_playback_refresh();
        }
        true
    }
}

fn playback_user_data_after_update(
    previous: Option<UserItemData>,
    update: &PlaybackStateUpdate,
) -> UserItemData {
    let mut data = previous.unwrap_or_default();
    if update.ended {
        data.played = true;
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
            list_item_id: "episode-1".to_string(),
            media_source_id: "source-1".to_string(),
            media_source_name: None,
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
