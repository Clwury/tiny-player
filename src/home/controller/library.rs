use super::HomeController;
use crate::{
    effects::WorkspaceIdentity,
    emby::{UserItems, UserView},
    home::{
        library::controller::{
            LibraryController, LibraryIntent, LibraryRequest, LibraryTransition, LibraryUpdate,
        },
        model::{library::library_item_types, navigation::NavigationChange},
    },
};

impl HomeController {
    pub(in crate::home) fn library_view(
        &self,
        view_id: &str,
    ) -> Option<crate::home::library::controller::LibraryVm<'_>> {
        self.libraries
            .get(view_id)
            .map(|library| library.view_model())
    }

    pub(in crate::home) fn open_library(
        &mut self,
        view: &UserView,
    ) -> Option<(NavigationChange, LibraryTransition)> {
        let item_types = library_item_types(view.collection_type.as_deref())?;
        let library = self.libraries.entry(view.id.clone()).or_insert_with(|| {
            LibraryController::new(item_types.clone(), view.id.clone(), self.identity.clone())
        });
        let transition = library.dispatch(
            LibraryIntent::Open(item_types.clone()),
            self.user_data.revision,
        );
        let change = self
            .navigation
            .push_library(view.id.clone(), view.name.clone(), item_types);
        Some((change, transition))
    }

    pub(in crate::home) fn dispatch_library(
        &mut self,
        view_id: &str,
        intent: LibraryIntent,
    ) -> Option<LibraryTransition> {
        Some(
            self.libraries
                .get_mut(view_id)?
                .dispatch(intent, self.user_data.revision),
        )
    }

    pub(in crate::home) fn complete_library(
        &mut self,
        request: &LibraryRequest,
        result: anyhow::Result<UserItems>,
        identity: &WorkspaceIdentity,
    ) -> Option<LibraryUpdate> {
        let mut update = self
            .libraries
            .get_mut(&request.view_id)?
            .complete(request, result, identity)?;
        if let Some(items) = update.received.take() {
            self.absorb_user_items_user_data(&items, request.user_data_revision);
        }
        Some(update)
    }
}
