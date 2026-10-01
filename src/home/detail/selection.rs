use super::controller::DetailIntent;
use super::*;
use crate::effects::DetailResource;
use crate::home::detail::binding::detail_binding;
use crate::home::track_preferences::detail_track_choices;

impl HomeContent {
    pub(in super::super) fn select_series_season(
        &mut self,
        season_id: String,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_detail_selection(DetailIntent::Season(season_id), true, cx);
    }

    pub(in super::super) fn toggle_series_season_select(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) =
            detail_binding(self.controller.detail_view(), &mut self.detail_resources)
        else {
            return;
        };
        let Some(seasons) = detail.model.seasons.as_ref() else {
            return;
        };
        if seasons.items.is_empty() {
            return;
        }

        let opening = detail.presentation.open_select != Some(SeriesDetailSelectKind::Season);
        if opening {
            let selected_index = detail
                .model
                .selected_season_id
                .as_deref()
                .and_then(|selected_id| {
                    seasons
                        .items
                        .iter()
                        .position(|season| season.id == selected_id)
                })
                .unwrap_or(0);
            detail
                .presentation
                .season_scroll_handle
                .scroll_to_item(selected_index);
        }
        detail.presentation.open_select = opening.then_some(SeriesDetailSelectKind::Season);
        cx.notify();
    }

    pub(in super::super) fn select_series_episode(
        &mut self,
        episode_id: String,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_detail_selection(DetailIntent::Episode(episode_id), true, cx);
    }

    pub(in super::super) fn toggle_series_media_source_select(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) =
            detail_binding(self.controller.detail_view(), &mut self.detail_resources)
        else {
            return;
        };
        if !detail.model.can_select_media_source() {
            return;
        }

        let opening = detail.presentation.open_select != Some(SeriesDetailSelectKind::MediaSource);
        if opening {
            render::reveal_two_line_option(
                &detail.presentation.media_source_scroll_handle,
                detail.model.selected_media_source_index().unwrap_or(0),
            );
        }
        detail.presentation.open_select = opening.then_some(SeriesDetailSelectKind::MediaSource);
        cx.notify();
    }

    pub(in super::super) fn toggle_series_subtitle_select(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(detail) =
            detail_binding(self.controller.detail_view(), &mut self.detail_resources)
        else {
            return;
        };
        if !detail.model.can_select_subtitle() {
            return;
        }

        let opening = detail.presentation.open_select != Some(SeriesDetailSelectKind::Subtitle);
        if opening {
            let saved_tracks = detail_track_choices(detail.model, &self.current_server, cx);
            render::reveal_two_line_option(
                &detail.presentation.subtitle_scroll_handle,
                detail
                    .model
                    .selected_subtitle_index(
                        PlaybackLanguagePreferences::get(cx).subtitle,
                        saved_tracks.subtitle.as_ref(),
                    )
                    .map(|index| index + 1)
                    .unwrap_or(0),
            );
        }
        detail.presentation.open_select = opening.then_some(SeriesDetailSelectKind::Subtitle);
        cx.notify();
    }

    pub(in super::super) fn select_series_media_source(
        &mut self,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_detail_selection(DetailIntent::MediaSource(index), false, cx);
    }

    pub(in super::super) fn select_series_subtitle(
        &mut self,
        index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_detail_selection(DetailIntent::Subtitle(index), true, cx);
    }

    fn dispatch_detail_selection(
        &mut self,
        intent: DetailIntent,
        close_select: bool,
        cx: &mut Context<Self>,
    ) {
        if self.detail_view().is_none() {
            return;
        }
        let Some(update) = self.controller.dispatch_detail(intent) else {
            return;
        };
        let detail = detail_binding(self.controller.detail_view(), &mut self.detail_resources)
            .expect("accepted detail selection keeps its resources");
        detail.presentation.apply_change(update.change);
        if update.playback_cancelled {
            detail.playback_task.cancel();
        }
        if close_select {
            detail.presentation.open_select = None;
        }
        if update.change.episodes_reset {
            detail.tasks.remove(&DetailResource::Episodes);
        }
        if update.load_episodes {
            self.load_series_episodes_if_needed(cx);
        }
        cx.notify();
    }
}
