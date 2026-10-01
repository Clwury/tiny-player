use super::HomeController;
use crate::home::{
    detail::controller::{DetailController, DetailRequest, DetailResponse, DetailUpdate},
    model::{navigation::NavigationChange, user_data::PendingUserData},
};
use crate::{
    effects::{DetailResource, WorkspaceIdentity},
    home::{
        detail::model::SeriesDetailModel,
        detail::{
            controller::DetailIntent,
            playback::{DetailPlaybackCommand, DetailPlaybackUpdate, SelectedPlayback},
        },
    },
    media::gateway::ResolvedPlayback,
};
/// Borrowed active-detail identity and read model. No mutable controller escapes.
pub(in crate::home) struct DetailModelView<'a> {
    pub(in crate::home) id: crate::home::detail::controller::DetailId,
    pub(in crate::home) model: &'a SeriesDetailModel,
}
impl HomeController {
    pub(in crate::home) fn detail_view(&self) -> Option<DetailModelView<'_>> {
        let detail = self.navigation.detail()?;
        Some(DetailModelView {
            id: detail.id(),
            model: detail.view_model(),
        })
    }
    pub(in crate::home) fn dispatch_detail(
        &mut self,
        intent: DetailIntent,
    ) -> Option<DetailUpdate> {
        self.navigation.detail_mut()?.dispatch(intent)
    }
    pub(in crate::home) fn begin_detail(
        &mut self,
        resource: DetailResource,
        identity: WorkspaceIdentity,
    ) -> Option<DetailRequest> {
        self.navigation
            .detail_mut()?
            .begin(resource, identity, self.user_data.revision)
    }
    pub(in crate::home) fn begin_detail_playback(
        &mut self,
        selected: Result<SelectedPlayback, String>,
        identity: WorkspaceIdentity,
    ) -> Result<Option<DetailPlaybackCommand>, String> {
        let Some(detail) = self.navigation.detail_mut() else {
            return Ok(None);
        };
        detail.begin_playback(selected, identity)
    }
    pub(in crate::home) fn complete_detail_playback(
        &mut self,
        command: DetailPlaybackCommand,
        result: anyhow::Result<ResolvedPlayback>,
        identity: &WorkspaceIdentity,
    ) -> Option<DetailPlaybackUpdate> {
        self.navigation
            .detail_mut()?
            .complete_playback(command, result, identity)
    }

    pub(in crate::home) fn open_detail(&mut self, detail: DetailController) -> NavigationChange {
        self.navigation
            .open_detail(detail, &self.played_video_versions)
    }
    pub(in crate::home) fn complete_detail(
        &mut self,
        request: &DetailRequest,
        result: anyhow::Result<DetailResponse>,
        identity: &crate::effects::WorkspaceIdentity,
    ) -> Option<DetailUpdate> {
        self.navigation.detail_mut()?.complete(
            request,
            result,
            identity,
            &mut self.user_data,
            PendingUserData {
                played: self.played_actions.has_pending(),
                favorites: &self.favorite_actions,
            },
        )
    }
}
