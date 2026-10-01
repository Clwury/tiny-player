use crate::{
    effects::RequestToken,
    emby::{MediaItem, MediaItems, ResumeItems, UserItem, UserItemData},
    home::{detail::model::SeriesDetailModel, model::notification::ActionNotification},
};

#[derive(Clone, Debug)]
pub(crate) struct PlayedRequest {
    pub(crate) item_id: String,
    pub(crate) series_id: Option<String>,
    pub(crate) whole_series: bool,
    pub(crate) played: bool,
    pub(crate) season_id: Option<String>,
    pub(crate) episode_id: Option<String>,
    pub(crate) detail_activation: Option<RequestToken>,
    pub(crate) user_data: Option<UserItemData>,
    pub(crate) notification: ActionNotification,
}

#[derive(Clone)]
pub(crate) struct PlayedCommand {
    pub(crate) request: PlayedRequest,
    pub(crate) token: RequestToken,
}

pub(crate) struct PlayedResponse {
    pub(crate) data: UserItemData,
    pub(crate) parent: Option<anyhow::Result<MediaItem>>,
    pub(crate) episodes: Option<anyhow::Result<MediaItems>>,
    pub(crate) episode: Option<anyhow::Result<MediaItem>>,
}

/// Temporary borrows of domain data for a mutation. No presentation handles or
/// copies of media responses are constructed; only references are collected.
pub(crate) struct PlayedContent<'a> {
    pub(crate) current: Option<&'a mut SeriesDetailModel>,
    pub(crate) history: Vec<&'a mut SeriesDetailModel>,
    pub(crate) items: Vec<&'a UserItem>,
    pub(crate) resume: &'a mut Option<ResumeItems>,
    pub(crate) detail_activation: Option<RequestToken>,
}

impl PlayedContent<'_> {
    pub(super) fn details(&self) -> impl Iterator<Item = &SeriesDetailModel> {
        self.current
            .as_deref()
            .into_iter()
            .chain(self.history.iter().map(|detail| &**detail))
    }
    pub(super) fn details_mut(&mut self) -> impl Iterator<Item = &mut SeriesDetailModel> {
        self.current
            .as_deref_mut()
            .into_iter()
            .chain(self.history.iter_mut().map(|detail| &mut **detail))
    }
}

pub(crate) struct PlayedUpdate {
    pub(crate) changed: bool,
    pub(crate) notification: ActionNotification,
    pub(crate) errors: Vec<String>,
}
