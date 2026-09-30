use super::HomeController;
use crate::{
    emby::{UserItems, VideoItemType},
    home::{
        favorites::controller::{
            FavoriteSectionVm, FavoritesIntent, FavoritesRequest, FavoritesUpdate,
        },
        model::user_data::PendingUserData,
    },
};

impl HomeController {
    pub(in crate::home) fn favorite_section(
        &self,
        item_type: VideoItemType,
    ) -> FavoriteSectionVm<'_> {
        self.favorites.view_model(item_type)
    }
    pub(in crate::home) fn has_favorites(&self) -> bool {
        self.favorites.has_items()
    }
    pub(in crate::home) fn invalidate_favorites(&mut self) {
        self.favorites.mark_dirty();
    }
    pub(in crate::home) fn dispatch_favorites(
        &mut self,
        item_type: VideoItemType,
        intent: FavoritesIntent,
    ) -> Option<FavoritesRequest> {
        self.favorites
            .dispatch(item_type, intent, self.user_data.revision)
    }
    pub(in crate::home) fn complete_favorites(
        &mut self,
        request: &FavoritesRequest,
        result: anyhow::Result<UserItems>,
        identity: &crate::effects::WorkspaceIdentity,
    ) -> Option<FavoritesUpdate> {
        self.favorites.complete(
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
