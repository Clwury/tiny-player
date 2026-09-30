//! Borrowed controls projection: shared user-data overrides and loading/selection
//! priorities are resolved here before rendering. No UI resources or IO.
use super::HomeController;
use crate::{
    emby::{MediaSource, MediaStream},
    home::model::detail::SeriesDetailModel,
    player::{SavedTrackChoices, TrackLanguage},
};

pub(in crate::home) struct DetailActionsVm {
    pub(in crate::home) favorite: bool,
    pub(in crate::home) played: bool,
    pub(in crate::home) enabled: bool,
}

pub(in crate::home) struct DetailControlsVm<'a> {
    pub(in crate::home) video_label: String,
    pub(in crate::home) subtitle_label: String,
    pub(in crate::home) media_sources: &'a [MediaSource],
    pub(in crate::home) subtitle_streams: Vec<&'a MediaStream>,
    pub(in crate::home) selected_source_index: Option<usize>,
    pub(in crate::home) selected_subtitle_index: Option<usize>,
    pub(in crate::home) media_source_select_enabled: bool,
    pub(in crate::home) subtitle_select_enabled: bool,
    pub(in crate::home) can_play: bool,
    pub(in crate::home) playback_position_seconds: Option<u64>,
    pub(in crate::home) is_series: bool,
    pub(in crate::home) actions: DetailActionsVm,
}

impl HomeController {
    pub(in crate::home) fn detail_actions(
        &self,
        detail: &SeriesDetailModel,
        whole_series: bool,
    ) -> DetailActionsVm {
        let item = if whole_series {
            detail.item.as_ref()
        } else {
            detail.selected_playback_item()
        };
        let data = if whole_series {
            self.effective_user_data(
                &detail.series_id,
                item.and_then(|item| item.user_data.as_ref()),
            )
        } else {
            item.and_then(|item| self.effective_user_data(&item.id, item.user_data.as_ref()))
        };
        DetailActionsVm {
            favorite: data.is_some_and(|data| data.is_favorite),
            played: data.is_some_and(|data| data.played),
            enabled: (whole_series || item.is_some()) && !self.user_data_pending(),
        }
    }

    pub(in crate::home) fn detail_controls<'a>(
        &self,
        detail: &'a SeriesDetailModel,
        subtitle_language: TrackLanguage,
        saved_tracks: &SavedTrackChoices,
    ) -> DetailControlsVm<'a> {
        DetailControlsVm {
            video_label: detail.selected_media_source_label(),
            subtitle_label: detail
                .selected_subtitle_label(subtitle_language, saved_tracks.subtitle.as_ref()),
            media_sources: detail.selected_media_sources().unwrap_or_default(),
            subtitle_streams: detail
                .selected_media_source()
                .map(|source| source.subtitle_streams())
                .unwrap_or_default(),
            selected_source_index: detail.selected_media_source_index(),
            selected_subtitle_index: detail
                .selected_subtitle_index(subtitle_language, saved_tracks.subtitle.as_ref()),
            media_source_select_enabled: detail.can_select_media_source(),
            subtitle_select_enabled: detail.can_select_subtitle(),
            can_play: detail.can_play(),
            playback_position_seconds: detail.playback_position_seconds(),
            is_series: detail.is_series(),
            actions: self.detail_actions(detail, false),
        }
    }
}
