use super::HomeController;
use crate::{
    effects::WorkspaceIdentity,
    emby::{MediaItem, MediaPerson, UserItems, VideoItemType},
    home::{
        library::controller::{
            LibraryIntent, LibraryRequest, LibrarySource, LibraryTransition, LibraryUpdate,
        },
        model::{navigation::NavigationChange, user_data::PendingUserData},
        person::controller::{PersonController, PersonRequest, PersonTransition, PersonVm},
    },
};

impl HomeController {
    pub(in crate::home) fn person_view(&self, person_id: &str) -> Option<PersonVm<'_>> {
        self.persons
            .get(person_id)
            .map(PersonController::view_model)
    }

    pub(in crate::home) fn open_person(
        &mut self,
        person: &MediaPerson,
    ) -> Option<(NavigationChange, PersonTransition)> {
        let id = person.id()?.to_string();
        let title = person.display_name();
        let person = self
            .persons
            .entry(id.clone())
            .or_insert_with(|| PersonController::new(id.clone(), self.identity.clone()));
        person.items.set_sort(self.items_sort);
        let transition = person.enter(self.user_data.revision);
        Some((self.navigation.push_person(id, title), transition))
    }

    pub(in crate::home) fn enter_person(&mut self, person_id: &str) -> Option<PersonTransition> {
        Some(
            self.persons
                .get_mut(person_id)?
                .enter(self.user_data.revision),
        )
    }

    pub(in crate::home) fn dispatch_person_items(
        &mut self,
        person_id: &str,
        intent: LibraryIntent,
    ) -> Option<LibraryTransition> {
        self.persons.get(person_id)?;
        let item_types = [VideoItemType::Movie, VideoItemType::Series];
        if let Some(changed) = self.apply_sort_intent(
            &intent,
            crate::media::ItemSortOptions::for_item_types(&item_types),
        ) {
            if !changed {
                return Some(LibraryTransition {
                    close_menu: true,
                    notify: true,
                    ..Default::default()
                });
            }
            let mut transition = self.persons.get_mut(person_id)?.items.dispatch(
                LibraryIntent::Open(item_types.to_vec()),
                self.user_data.revision,
            );
            transition.cancel = true;
            return Some(transition);
        }
        Some(
            self.persons
                .get_mut(person_id)?
                .items
                .dispatch(intent, self.user_data.revision),
        )
    }

    pub(in crate::home) fn complete_person_items(
        &mut self,
        request: &LibraryRequest,
        result: anyhow::Result<UserItems>,
        identity: &WorkspaceIdentity,
    ) -> Option<LibraryUpdate> {
        let LibrarySource::Person(id) = &request.source else {
            return None;
        };
        let mut update = self
            .persons
            .get_mut(id)?
            .items
            .complete(request, result, identity)?;
        if let Some(items) = update.received.take() {
            self.absorb_user_items_user_data(&items, request.user_data_revision);
        }
        Some(update)
    }

    pub(in crate::home) fn complete_person(
        &mut self,
        request: &PersonRequest,
        result: anyhow::Result<MediaItem>,
        identity: &WorkspaceIdentity,
    ) -> Option<Result<(), String>> {
        let result = self
            .persons
            .get_mut(&request.person_id)?
            .complete(request, result, identity)?;
        Some(result.map(|item| {
            self.user_data.absorb(
                &item.id,
                item.user_data.as_ref(),
                request.user_data_revision,
                PendingUserData {
                    played: self.played_actions.has_pending(),
                    favorites: &self.favorite_actions,
                },
            );
        }))
    }
}
