pub(in crate::home) mod model;
#[cfg(test)]
use crate::home::detail::test_fixture::detail_binding;
mod actions;
pub(in crate::home) mod binding;
pub(in crate::home) mod controller;
mod effect;
mod images;
mod launch;
mod overview;
pub(in crate::home) mod playback;
#[cfg(test)]
mod playback_tests;
mod render;
#[cfg(test)]
pub(in crate::home) mod test_fixture;

use super::notification::{HOME_RESUME_DETAIL_NOTIFICATION_KEY, NotificationScope};
pub(crate) use binding::{DetailView, SeriesDetailSelectKind};
#[cfg(test)]
use test_fixture::DetailFixture;

use gpui::{AppContext as _, ClickEvent, Context, MouseDownEvent, Window};

use crate::{
    emby::{ResumeItem, UserItem},
    media::{PlaybackLanguagePreferences, PlaybackTrackPreferenceKey},
    player::{EmbyPlaybackContext, PlaybackRequest, playback_initial_position_seconds},
};

use super::{HomeContent, HomeContentEvent};
#[cfg(test)]
use {
    self::playback::SelectedPlayback,
    super::LoadState,
    crate::emby::{MediaItem, MediaItems},
    crate::media::{PlaybackTrack, SavedTrackChoices, gateway::ResolvedPlayback},
    crate::server::CachedServer,
};

const DETAIL_ITEM_NOTIFICATION_KEY: &str = "detail:item";
const DETAIL_SIMILAR_NOTIFICATION_KEY: &str = "detail:similar";
const DETAIL_SEASONS_NOTIFICATION_KEY: &str = "detail:seasons";
const DETAIL_NEXT_UP_NOTIFICATION_KEY: &str = "detail:next-up";
const DETAIL_EPISODES_NOTIFICATION_KEY: &str = "detail:episodes";
const DETAIL_PLAYBACK_NOTIFICATION_KEY: &str = "detail:playback";

#[path = "detail/loading.rs"]
mod loading;
#[path = "detail/navigation.rs"]
mod navigation;
#[path = "detail/selection.rs"]
mod selection;

#[cfg(test)]
#[path = "detail/track_preferences_tests.rs"]
mod track_preferences_tests;

#[cfg(test)]
fn playback_subtitle_tracks(
    source: &crate::emby::MediaSource,
    server: &CachedServer,
    item_id: &str,
    media_source_id: &str,
) -> Vec<PlaybackTrack> {
    crate::player::adapter::subtitles::playback_subtitle_tracks_for_source(
        source,
        server,
        item_id,
        media_source_id,
    )
}

#[cfg(test)]
fn selected_playback(
    detail: DetailView<'_>,
    server: &CachedServer,
    languages: PlaybackLanguagePreferences,
    saved: &SavedTrackChoices,
) -> Result<SelectedPlayback, String> {
    effect::selected_playback(
        detail.model,
        &crate::player::adapter::EmbyPlaybackGateway {
            client: crate::emby::EmbyClient::new("test".into()).unwrap(),
            server: server.clone(),
        },
        languages,
        saved,
    )
}
#[cfg(test)]
fn playback_queue(
    detail: DetailView<'_>,
    selected: &crate::emby::MediaItem,
    title: &str,
) -> crate::player::PlaybackQueue {
    playback::playback_queue(detail.model, selected, title)
}

#[cfg(test)]
fn begin_prepared_playback(
    page: &mut HomeContent,
    selected: SelectedPlayback,
) -> playback::DetailPlaybackCommand {
    let identity = page.request_identity();
    detail_binding(
        page.controller.test_state_mut().navigation,
        &mut page.detail_resources,
    )
    .unwrap()
    .controller
    .begin_playback(Ok(selected), identity)
    .unwrap()
    .unwrap()
}

#[cfg(test)]
fn restart_detail_request(
    page: &mut HomeContent,
    resource: crate::effects::DetailResource,
    revision: Option<u64>,
) -> controller::DetailRequest {
    use crate::effects::DetailResource;
    let identity = page.request_identity();
    let revision = revision.unwrap_or_else(|| page.controller.user_data_request_revision());
    let detail = detail_binding(
        page.controller.test_state_mut().navigation,
        &mut page.detail_resources,
    )
    .unwrap();
    detail.tasks.remove(&resource);
    let effects = &mut detail.controller.state.effects;
    *match resource {
        DetailResource::Item => &mut effects.item,
        DetailResource::Similar => &mut effects.similar,
        DetailResource::Seasons => &mut effects.seasons,
        DetailResource::NextUp => &mut effects.next_up,
        DetailResource::Episodes => &mut effects.episodes,
        DetailResource::ResumeSources => &mut effects.resume_sources,
    } = LoadState::Idle;
    detail
        .controller
        .begin(resource, identity, revision)
        .unwrap()
}

#[cfg(test)]
mod tests;
