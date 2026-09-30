//! Fixture-only borrows for existing GPUI regression setup and assertions.
//! Production readers use selectors; production writers use intents/results.
use super::*;

pub(in crate::home) struct HomeTestState<'a> {
    pub(in crate::home) navigation: &'a HomeNavigation,
    pub(in crate::home) feed: &'a FeedController,
    pub(in crate::home) libraries: &'a HashMap<String, LibraryController>,
    pub(in crate::home) favorites: &'a FavoritesController,
    pub(in crate::home) search: &'a SearchController,
    pub(in crate::home) user_data: &'a UserDataState,
    pub(in crate::home) played_video_versions: &'a HashMap<String, VideoVersion>,
    pub(in crate::home) favorite_actions: &'a FavoriteActions,
    pub(in crate::home) played_actions: &'a PlayedController,
    pub(in crate::home) resume_actions: &'a ResumeActions,
}
pub(in crate::home) struct HomeTestStateMut<'a> {
    pub(in crate::home) navigation: &'a mut HomeNavigation,
    pub(in crate::home) feed: &'a mut FeedController,
    pub(in crate::home) libraries: &'a mut HashMap<String, LibraryController>,
    pub(in crate::home) favorites: &'a mut FavoritesController,
    pub(in crate::home) search: &'a mut SearchController,
    pub(in crate::home) user_data: &'a mut UserDataState,
    pub(in crate::home) played_video_versions: &'a mut HashMap<String, VideoVersion>,
    pub(in crate::home) played_actions: &'a mut PlayedController,
}
impl HomeController {
    pub(in crate::home) fn test_absorb_user_items(
        &mut self,
        items: &crate::emby::UserItems,
        revision: u64,
    ) {
        self.absorb_user_items_user_data(items, revision);
    }

    pub(in crate::home) fn test_state(&self) -> HomeTestState<'_> {
        HomeTestState {
            navigation: &self.navigation,
            feed: &self.feed,
            libraries: &self.libraries,
            favorites: &self.favorites,
            search: &self.search,
            user_data: &self.user_data,
            played_video_versions: &self.played_video_versions,
            favorite_actions: &self.favorite_actions,
            played_actions: &self.played_actions,
            resume_actions: &self.resume_actions,
        }
    }
    pub(in crate::home) fn test_state_mut(&mut self) -> HomeTestStateMut<'_> {
        HomeTestStateMut {
            navigation: &mut self.navigation,
            feed: &mut self.feed,
            libraries: &mut self.libraries,
            favorites: &mut self.favorites,
            search: &mut self.search,
            user_data: &mut self.user_data,
            played_video_versions: &mut self.played_video_versions,
            played_actions: &mut self.played_actions,
        }
    }
}
