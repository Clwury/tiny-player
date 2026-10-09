use super::HomeController;
use crate::{
    effects::WorkspaceIdentity,
    emby::{MediaGenre, UserItems, VideoItemType},
    home::{
        library::controller::{
            LibraryController, LibraryIntent, LibraryRequest, LibrarySource, LibraryTransition,
            LibraryUpdate, LibraryVm,
        },
        model::navigation::NavigationChange,
    },
};

impl HomeController {
    pub(in crate::home) fn genre_view(&self, genre_key: &str) -> Option<LibraryVm<'_>> {
        self.genres
            .get(genre_key)
            .map(LibraryController::view_model)
    }

    pub(in crate::home) fn open_genre(
        &mut self,
        genre: &MediaGenre,
    ) -> Option<(NavigationChange, LibraryTransition)> {
        let genre = genre.normalized()?;
        let key = genre.key();
        let title = genre.name.clone();
        let controller = self
            .genres
            .entry(key.clone())
            .or_insert_with(|| LibraryController::for_genre(genre, self.identity.clone()));
        controller.set_sort(self.items_sort);
        let transition = controller.dispatch(
            LibraryIntent::Open(vec![VideoItemType::Movie, VideoItemType::Series]),
            self.user_data.revision,
        );
        Some((self.navigation.push_genre(key, title), transition))
    }

    pub(in crate::home) fn dispatch_genre_items(
        &mut self,
        genre_key: &str,
        intent: LibraryIntent,
    ) -> Option<LibraryTransition> {
        self.genres.get(genre_key)?;
        let item_types = [VideoItemType::Movie, VideoItemType::Series];
        if let Some(changed) = self.apply_sort_intent(&intent, crate::media::ItemSortOptions::ALL) {
            if !changed {
                return Some(LibraryTransition {
                    close_menu: true,
                    notify: true,
                    ..Default::default()
                });
            }
            let mut transition = self.genres.get_mut(genre_key)?.dispatch(
                LibraryIntent::Open(item_types.to_vec()),
                self.user_data.revision,
            );
            transition.cancel = true;
            return Some(transition);
        }
        Some(
            self.genres
                .get_mut(genre_key)?
                .dispatch(intent, self.user_data.revision),
        )
    }

    pub(in crate::home) fn complete_genre_items(
        &mut self,
        request: &LibraryRequest,
        result: anyhow::Result<UserItems>,
        identity: &WorkspaceIdentity,
    ) -> Option<LibraryUpdate> {
        let LibrarySource::Genre(key) = &request.source else {
            return None;
        };
        let mut update = self
            .genres
            .get_mut(key)?
            .complete(request, result, identity)?;
        if let Some(items) = update.received.take() {
            self.absorb_user_items_user_data(&items, request.user_data_revision);
        }
        Some(update)
    }
}
