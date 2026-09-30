use super::HomeController;
use crate::{
    effects::WorkspaceIdentity,
    emby::{UserItems, UserView},
    home::{
        feed::{FeedIntent, FeedRequest, FeedResponse, FeedUpdate, HomeFeedVm, LatestRowVm},
        model::user_data::PendingUserData,
    },
};

impl HomeController {
    pub(in crate::home) fn feed_view(&self) -> HomeFeedVm<'_> {
        self.feed.view_model()
    }
    pub(in crate::home) fn latest_row(&self, view_id: &str) -> LatestRowVm<'_> {
        self.feed.latest_row(view_id)
    }
    pub(in crate::home) fn latest_items(&self) -> impl Iterator<Item = &UserItems> {
        self.feed
            .state
            .user_view_items_rows
            .values()
            .filter_map(|row| row.items.as_ref())
    }
    pub(in crate::home) fn user_view(&self, view_id: &str) -> Option<&UserView> {
        self.feed
            .state
            .user_views
            .as_ref()?
            .items
            .iter()
            .find(|view| view.id == view_id)
    }
    pub(in crate::home) fn dispatch_feed(&mut self, intent: FeedIntent) -> Vec<FeedRequest> {
        self.feed.dispatch(intent)
    }
    pub(in crate::home) fn pump_latest(&mut self) -> Vec<FeedRequest> {
        self.feed.pump_latest()
    }
    pub(in crate::home) fn complete_feed(
        &mut self,
        request: &FeedRequest,
        revision: u64,
        response: FeedResponse,
        identity: &WorkspaceIdentity,
    ) -> FeedUpdate {
        if !request.token.is_for(identity) {
            return FeedUpdate::Ignored;
        }
        let update = self.feed.complete(request, response);
        let pending = PendingUserData {
            played: self.played_actions.has_pending(),
            favorites: &self.favorite_actions,
        };
        match &update {
            FeedUpdate::Resume => {
                if let Some(items) = &self.feed.state.resume_items {
                    self.user_data.absorb_resume(items, revision, pending);
                }
            }
            FeedUpdate::Latest(id) => {
                if let Some(items) = self
                    .feed
                    .state
                    .user_view_items_rows
                    .get(id)
                    .and_then(|row| row.items.as_ref())
                {
                    self.user_data.absorb_items(items, revision, pending);
                }
            }
            _ => {}
        }
        update
    }
}
