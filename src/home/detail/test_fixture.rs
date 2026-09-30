use super::controller::{DetailController, DetailId};
use super::state::*;
use crate::effects::WorkspaceIdentity;
use crate::effects::{DetailResource, EffectHandle};
use crate::emby::UserItem;
use crate::home::model::detail::{DetailChange, SeriesDetailModel};
use crate::home::model::navigation::HomeNavigation;
use crate::player::SavedTrackChoices;
use std::collections::HashMap;

#[derive(Debug)]
pub(crate) struct DetailFixture {
    pub(crate) controller: DetailController,
    pub(crate) playback_task: EffectHandle<gpui::Task<()>>,
    pub(crate) tasks: HashMap<DetailResource, EffectHandle<gpui::Task<()>>>,
    pub(crate) presentation: SeriesDetailPresentation,
}

impl DetailFixture {
    pub(crate) fn view(&self) -> DetailView<'_> {
        DetailView {
            model: self.controller.view_model(),
            presentation: &self.presentation,
        }
    }
    pub(crate) fn into_parts(self) -> (DetailController, DetailResources) {
        (
            self.controller,
            DetailResources {
                tasks: self.tasks,
                playback_task: self.playback_task,
                presentation: self.presentation,
            },
        )
    }

    fn from_model(model: SeriesDetailModel, identity: WorkspaceIdentity) -> Self {
        Self {
            controller: DetailController::new(model, identity),
            tasks: HashMap::new(),
            playback_task: Default::default(),
            presentation: SeriesDetailPresentation::default(),
        }
    }
    pub(crate) fn from_user_item(item: &UserItem, identity: WorkspaceIdentity) -> Option<Self> {
        SeriesDetailModel::from_user_item(item).map(|model| Self::from_model(model, identity))
    }
    #[cfg(test)]
    pub(crate) fn new_series(item: &UserItem) -> Self {
        Self::from_model(
            SeriesDetailModel::new_series(item),
            WorkspaceIdentity::default(),
        )
    }
    #[cfg(test)]
    pub(crate) fn new_movie(item: &UserItem) -> Self {
        Self::from_model(
            SeriesDetailModel::new_movie(item),
            WorkspaceIdentity::default(),
        )
    }

    pub(super) fn apply_change(&mut self, change: DetailChange) {
        self.presentation.apply_change(change);
    }

    pub(crate) fn apply_playback_update(
        &mut self,
        update: &crate::player::PlaybackStateUpdate,
        user_data: &crate::emby::UserItemData,
    ) {
        let update = self.controller.apply_playback_update(update, user_data);
        self.apply_change(update.change);
        if update.playback_cancelled {
            self.playback_task.cancel();
        }
    }
    pub(crate) fn selected_track_choices(
        &self,
        server: &crate::server::CachedServer,
        cx: &gpui::App,
    ) -> SavedTrackChoices {
        crate::home::track_preferences::detail_track_choices(
            self.controller.view_model(),
            server,
            cx,
        )
    }
}

/// A temporary runner borrow; it owns neither controller nor presentation and
/// is never stored in a page or history entry. Renderers only receive DetailView.
pub(crate) struct DetailBinding<'a> {
    pub(crate) controller: &'a mut DetailController,
    pub(crate) playback_task: &'a mut EffectHandle<gpui::Task<()>>,
    pub(crate) tasks: &'a mut HashMap<DetailResource, EffectHandle<gpui::Task<()>>>,
    pub(crate) presentation: &'a mut SeriesDetailPresentation,
}

pub(crate) fn detail_binding<'a>(
    navigation: &'a mut HomeNavigation,
    resources: &'a mut HashMap<DetailId, DetailResources>,
) -> Option<DetailBinding<'a>> {
    let controller = navigation.detail_mut()?;
    let resources = resources.get_mut(&controller.id())?;
    Some(DetailBinding {
        controller,
        playback_task: &mut resources.playback_task,
        tasks: &mut resources.tasks,
        presentation: &mut resources.presentation,
    })
}
