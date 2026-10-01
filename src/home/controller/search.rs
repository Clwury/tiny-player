use super::HomeController;
use crate::{
    home::{
        search::controller::{
            SearchIntent, SearchRequestContext, SearchTransition, SearchUpdate, SearchVm,
        },
        search::model::SearchPage,
    },
    search_history::SearchHistory,
};

impl HomeController {
    pub(in crate::home) fn search_view(&self) -> SearchVm<'_> {
        self.search.view_model()
    }
    pub(in crate::home) fn restore_search_history(&mut self, history: SearchHistory) {
        self.search.restore_history(history);
    }
    pub(in crate::home) fn dispatch_search(&mut self, intent: SearchIntent) -> SearchTransition {
        self.search.dispatch(intent, self.user_data.revision)
    }
    pub(in crate::home) fn complete_search(
        &mut self,
        request: &SearchRequestContext,
        result: anyhow::Result<SearchPage>,
        identity: &crate::effects::WorkspaceIdentity,
    ) -> Option<SearchUpdate> {
        let update = self.search.complete(request, result, identity)?;
        if let Some(items) = &update.received {
            self.absorb_user_items_user_data(items, request.user_data_revision);
        }
        Some(update)
    }
}
