use super::*;

impl HomeContent {
    pub(in super::super) fn select_series_season(
        &mut self,
        season_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        if detail.selected_season_id.as_deref() == Some(season_id.as_str()) {
            detail.open_select = None;
            cx.notify();
            return;
        }

        detail.selected_season_id = Some(season_id);
        detail.preferred_episode_id = None;
        detail.clear_preferred_season_hint();
        detail.open_select = None;
        detail.reset_episode_selection();
        self.load_series_episodes_if_needed(cx);
        cx.notify();
    }

    pub(in super::super) fn toggle_series_season_select(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        let Some(seasons) = detail.seasons.as_ref() else {
            return;
        };
        if seasons.items.is_empty() {
            return;
        }

        let opening = detail.open_select != Some(SeriesDetailSelectKind::Season);
        if opening {
            let selected_index = detail
                .selected_season_id
                .as_deref()
                .and_then(|selected_id| {
                    seasons
                        .items
                        .iter()
                        .position(|season| season.id == selected_id)
                })
                .unwrap_or(0);
            detail.season_scroll_handle.scroll_to_item(selected_index);
        }
        detail.open_select = opening.then_some(SeriesDetailSelectKind::Season);
        cx.notify();
    }

    pub(in super::super) fn select_series_episode(
        &mut self,
        episode_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        let exists = detail.episodes.as_ref().is_some_and(|episodes| {
            episodes
                .items
                .iter()
                .any(|episode| episode.id == episode_id)
        });
        if !exists {
            return;
        }

        detail.preferred_episode_id = Some(episode_id.clone());
        detail.apply_selected_episode(Some(episode_id));
        detail.open_select = None;
        cx.notify();
    }

    pub(in super::super) fn toggle_series_media_source_select(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        let source_count = detail
            .selected_playback_item()
            .and_then(|item| item.media_sources.as_ref())
            .map(Vec::len)
            .unwrap_or(0);
        if source_count == 0 {
            return;
        }

        let opening = detail.open_select != Some(SeriesDetailSelectKind::MediaSource);
        if opening {
            detail
                .media_source_scroll_handle
                .scroll_to_item(detail.selected_media_source_index().unwrap_or(0));
        }
        detail.open_select = opening.then_some(SeriesDetailSelectKind::MediaSource);
        cx.notify();
    }

    pub(in super::super) fn toggle_series_subtitle_select(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        let subtitle_count = detail
            .selected_media_source()
            .map(|source| source.subtitle_streams().len())
            .unwrap_or(0);
        if subtitle_count == 0 {
            return;
        }

        let opening = detail.open_select != Some(SeriesDetailSelectKind::Subtitle);
        if opening {
            detail
                .subtitle_scroll_handle
                .scroll_to_item(detail.selected_subtitle_index().unwrap_or(0));
        }
        detail.open_select = opening.then_some(SeriesDetailSelectKind::Subtitle);
        cx.notify();
    }

    pub(in super::super) fn select_series_media_source(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        let source_count = detail
            .selected_playback_item()
            .and_then(|item| item.media_sources.as_ref())
            .map(Vec::len)
            .unwrap_or(0);
        if index >= source_count {
            return;
        }

        detail.selected_media_source_index = Some(index);
        detail.selected_subtitle_index = None;
        detail
            .subtitle_scroll_handle
            .set_offset(point(px(0.0), px(0.0)));
        detail.open_select = None;
        detail.reset_playback_request();
        detail.sync_media_source_selection();
        cx.notify();
    }

    pub(in super::super) fn select_series_subtitle(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) = self.series_detail.as_mut() else {
            return;
        };
        let subtitle_count = detail
            .selected_media_source()
            .map(|source| source.subtitle_streams().len())
            .unwrap_or(0);
        if index >= subtitle_count {
            return;
        }

        detail.selected_subtitle_index = Some(index);
        detail.open_select = None;
        cx.notify();
    }
}
