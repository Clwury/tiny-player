use super::HomeController;
use crate::home::model::favorites::FavoriteItemType;
use crate::{
    emby::UserItems,
    home::{
        favorites::controller::{
            FavoriteSectionVm, FavoritesIntent, FavoritesRequest, FavoritesUpdate,
        },
        model::{
            navigation::{HomeRoot, HomeRoute},
            user_data::PendingUserData,
        },
    },
};

impl HomeController {
    pub(in crate::home) fn favorite_section(
        &self,
        item_type: FavoriteItemType,
    ) -> FavoriteSectionVm<'_> {
        if self.route() == &HomeRoute::Root(HomeRoot::Favorites) {
            self.favorites.overview_view_model(item_type)
        } else {
            self.favorites.view_model(item_type)
        }
    }
    pub(in crate::home) fn favorite_overview_section(
        &self,
        item_type: FavoriteItemType,
    ) -> FavoriteSectionVm<'_> {
        self.favorites.overview_view_model(item_type)
    }
    pub(in crate::home) fn has_favorites(&self) -> bool {
        self.favorites.has_items()
    }
    pub(in crate::home) fn invalidate_favorites(&mut self) {
        self.favorites.mark_dirty();
    }
    pub(in crate::home) fn dispatch_favorites(
        &mut self,
        item_type: FavoriteItemType,
        intent: FavoritesIntent,
    ) -> Option<FavoritesRequest> {
        let sort_intent = match intent {
            FavoritesIntent::SortBy(sort_by) => Some(
                crate::home::library::controller::LibraryIntent::SortBy(sort_by),
            ),
            FavoritesIntent::SortOrder(sort_order) => Some(
                crate::home::library::controller::LibraryIntent::SortOrder(sort_order),
            ),
            _ => None,
        };
        if let Some(sort_intent) = sort_intent {
            if !self.apply_sort_intent(
                &sort_intent,
                crate::media::ItemSortOptions::for_item_types(item_type.video_types()),
            )? {
                return None;
            }
            return self.favorites.dispatch(
                item_type,
                FavoritesIntent::Enter,
                self.user_data.revision,
            );
        }
        if self.route() == &HomeRoute::Root(HomeRoot::Favorites) {
            self.favorites
                .dispatch_overview(item_type, intent, self.user_data.revision)
        } else {
            self.favorites
                .dispatch(item_type, intent, self.user_data.revision)
        }
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
