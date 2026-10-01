//! Detail selection rules; state remains owned by SeriesDetailModel.
use super::*;

impl SeriesDetailModel {
    pub(crate) fn selected_media_source(&self) -> Option<&MediaSource> {
        let sources = self.selected_media_sources()?;
        let index = self.selected_media_source_index()?;
        sources.get(index)
    }

    pub(crate) fn selected_media_source_index(&self) -> Option<usize> {
        let sources = self.selected_media_sources()?;
        self.manual_video_version
            .as_ref()
            .and_then(|version| version.find_source(sources))
            .or_else(|| {
                if self.is_resume_playback_item_selected() {
                    // Like Tsukimi, restore the version matcher or use the first
                    // PlaybackInfo source. Resume.Id identifies an item, not its
                    // previously selected version on servers that group versions.
                    self.resume_video_version
                        .as_ref()
                        .and_then(|version| version.find_source(sources))
                        .or_else(|| (!sources.is_empty()).then_some(0))
                } else {
                    preferred_media_source_index(sources)
                }
            })
    }

    pub(crate) fn resume_media_item_id(&self) -> Option<&str> {
        self.resume_media_item_id.as_deref()
    }

    pub(super) fn is_resume_playback_item_selected(&self) -> bool {
        let Some(resume_id) = self.resume_media_item_id.as_deref() else {
            return false;
        };
        self.selected_playback_item().is_some_and(|item| {
            self.is_movie()
                || item.id == resume_id
                || item.media_sources.as_ref().is_some_and(|sources| {
                    sources
                        .iter()
                        .any(|source| source.matches_item_id(resume_id))
                })
        })
    }

    pub(crate) fn selected_media_sources(&self) -> Option<&[MediaSource]> {
        if self.is_resume_playback_item_selected()
            && let Some(sources) = self.resume_media_sources.as_deref()
        {
            return Some(sources);
        }
        self.selected_playback_item()?.media_sources.as_deref()
    }

    pub(crate) fn video_sources_loading(&self) -> bool {
        self.is_resume_playback_item_selected() && self.effects.resume_sources == LoadState::Loading
    }

    pub(crate) fn select_media_source(&mut self, index: usize) -> DetailChange {
        let Some(source) = self
            .selected_media_sources()
            .and_then(|sources| sources.get(index))
        else {
            return DetailChange::default();
        };
        self.manual_video_version = Some(VideoVersion::from_source(source));
        self.selected_media_source_index = Some(index);
        self.reset_playback_request();
        let mut change = self.sync_media_source_selection();
        change.source_selected = true;
        change
    }

    pub(crate) fn selected_subtitle_index(
        &self,
        language: TrackLanguage,
        preference: Option<&SavedTrackChoice>,
    ) -> Option<usize> {
        let source = self.selected_media_source()?;
        let streams = source.subtitle_streams();
        if let Some(preference) = preference {
            let tracks = streams
                .iter()
                .enumerate()
                .filter_map(|(index, stream)| PlaybackTrack::from_subtitle_stream(stream, index))
                .collect::<Vec<_>>();
            if let Some(track) = preference.resolve(&tracks) {
                return track.and_then(|track| {
                    streams.iter().position(|stream| {
                        stream.index.and_then(|index| usize::try_from(index).ok())
                            == Some(track.stream_index)
                    })
                });
            }
        }
        language.preferred_subtitle_stream_position(source)
    }

    pub(crate) fn track_preference_key(&self) -> Option<PlaybackTrackPreferenceKey> {
        let item = self.selected_playback_item()?;
        let source = self.selected_media_source()?;
        Some(PlaybackTrackPreferenceKey {
            item_id: source.playback_item_id(&item.id).to_string(),
            media_source_id: source.id.clone()?,
        })
    }

    pub(crate) fn pending_subtitle_choice(&self) -> Option<&SavedTrackChoice> {
        self.pending_subtitle_choices
            .get(&self.track_preference_key()?)
    }

    /// A detail-local draft wins over the saved choice for this source; audio
    /// remains untouched. Reading choices never consumes the draft.
    pub(crate) fn selected_track_choices(&self, mut saved: SavedTrackChoices) -> SavedTrackChoices {
        if let Some(subtitle) = self.pending_subtitle_choice() {
            saved.subtitle = Some(subtitle.clone());
        }
        saved
    }

    pub(crate) fn can_select_media_source(&self) -> bool {
        !self.video_sources_loading()
            && self
                .selected_media_sources()
                .is_some_and(|sources| !sources.is_empty())
    }

    pub(crate) fn can_select_subtitle(&self) -> bool {
        !self.video_sources_loading()
            && self
                .selected_media_source()
                .is_some_and(|source| !source.subtitle_streams().is_empty())
    }

    pub(crate) fn can_play(&self) -> bool {
        !self.playback_loading
            && !self.video_sources_loading()
            && self.selected_playback_item().is_some()
            && self
                .selected_media_source()
                .and_then(|source| source.id.as_deref())
                .is_some_and(|id| !id.trim().is_empty())
    }

    pub(crate) fn selected_media_source_label(&self) -> String {
        if self.video_sources_loading() {
            return "正在加载视频源…".to_string();
        }
        let Some(sources) = self.selected_media_sources() else {
            return "暂无视频源".to_string();
        };
        let Some(index) = self.selected_media_source_index() else {
            return "暂无视频源".to_string();
        };
        sources
            .get(index)
            .map(|source| source.name_label(index))
            .unwrap_or_else(|| "暂无视频源".to_string())
    }

    pub(crate) fn selected_subtitle_label(
        &self,
        language: TrackLanguage,
        preference: Option<&SavedTrackChoice>,
    ) -> String {
        let Some(source) = self.selected_media_source() else {
            return "无字幕".to_string();
        };
        let subtitles = source.subtitle_streams();
        if subtitles.is_empty() {
            return "无字幕".to_string();
        }
        if preference == Some(&SavedTrackChoice::Off) {
            return "Off".to_string();
        }
        let Some(index) = self.selected_subtitle_index(language, preference) else {
            return "无字幕".to_string();
        };
        subtitles
            .get(index)
            .map(|stream| stream.display_title_label(index))
            .unwrap_or_else(|| "无字幕".to_string())
    }
}

fn preferred_media_source_index(sources: &[MediaSource]) -> Option<usize> {
    if sources.is_empty() {
        return None;
    }

    sources
        .iter()
        .position(MediaSource::is_default_source)
        .or_else(|| {
            sources
                .iter()
                .position(MediaSource::has_default_video_stream)
        })
        .or(Some(0))
}
