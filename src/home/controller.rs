//! Workspace business owner. Presentation and cancellable delivery handles stay
//! on HomeContent; reducers below operate without GPUI or IO. Drop this controller
//! at workspace release. Feature intents and accepted results are its write paths.
mod cards;
mod detail;
mod detail_controls;
mod favorites;
mod feed;
mod library;
mod mutations;
mod navigation;
mod playback;
mod search;
mod selectors;
mod snapshot;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use super::{
    favorites::{FavoritesController, actions::FavoriteActions},
    feed::FeedController,
    library::controller::LibraryController,
    model::{navigation::HomeNavigation, user_data::UserDataState},
    played::PlayedController,
    resume_actions::controller::ResumeActions,
    search::controller::SearchController,
    video_version::VideoVersion,
};
pub(in crate::home) use detail::DetailModelView;
pub(in crate::home) use mutations::{FavoriteIntent, PlayedIntent};
pub(in crate::home) use navigation::{NavigationIntent, OpenDetailIntent};

use crate::effects::{RequestScope, RequestSlot, WorkspaceIdentity};
use std::collections::HashMap;

#[derive(Debug)]
pub(super) struct HomeController {
    identity: WorkspaceIdentity,
    navigation: HomeNavigation,
    feed: FeedController,
    libraries: HashMap<String, LibraryController>,
    favorites: FavoritesController,
    search: SearchController,
    user_data: UserDataState,
    played_video_versions: HashMap<String, VideoVersion>,
    favorite_actions: FavoriteActions,
    played_actions: PlayedController,
    resume_actions: ResumeActions,
    // Stop-report completion may refresh data once for the latest return only.
    playback_refresh: RequestSlot,
}

impl HomeController {
    pub(super) fn new(identity: WorkspaceIdentity) -> Self {
        Self {
            navigation: HomeNavigation::default(),
            feed: FeedController::new(identity.clone()),
            libraries: HashMap::new(),
            favorites: FavoritesController::new(identity.clone()),
            search: SearchController::new(identity.clone()),
            user_data: UserDataState::default(),
            played_video_versions: HashMap::new(),
            favorite_actions: FavoriteActions::new(identity.clone()),
            played_actions: PlayedController::new(identity.clone()),
            resume_actions: ResumeActions::new(identity.clone()),
            playback_refresh: RequestSlot::new(RequestScope::HomePlaybackRefresh, identity.clone()),
            identity,
        }
    }
}
