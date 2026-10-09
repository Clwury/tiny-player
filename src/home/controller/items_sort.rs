use super::HomeController;
use crate::{
    emby::{UserItemsSort, VideoItemType},
    home::{library::controller::LibraryIntent, model::navigation::HomeRoute},
    media::{ItemSortOptions, ItemSortPreferences},
};

impl HomeController {
    pub(in crate::home) fn set_items_sort(&mut self, preferences: ItemSortPreferences) -> bool {
        if self.items_sort == preferences {
            return false;
        }
        self.items_sort = preferences;
        self.favorites.set_sort(preferences);
        for library in self.libraries.values_mut().chain(self.genres.values_mut()) {
            library.set_sort(preferences);
        }
        for person in self.persons.values_mut() {
            person.items.set_sort(preferences);
        }
        true
    }

    pub(in crate::home) fn current_items_sort_is_available(&self, sort_by: UserItemsSort) -> bool {
        let options = match self.route() {
            HomeRoute::Library { item_types, .. } => ItemSortOptions::for_item_types(item_types),
            HomeRoute::FavoriteItems { item_type } => {
                ItemSortOptions::for_item_types(item_type.video_types())
            }
            HomeRoute::Person { .. } => {
                ItemSortOptions::for_item_types(&[VideoItemType::Movie, VideoItemType::Series])
            }
            HomeRoute::Genre { .. } => ItemSortOptions::ALL,
            _ => return false,
        };
        options.contains(sort_by)
    }

    pub(super) fn apply_sort_intent(
        &mut self,
        intent: &LibraryIntent,
        options: ItemSortOptions,
    ) -> Option<bool> {
        let preferences = match *intent {
            LibraryIntent::SortBy(sort_by) if !options.contains(sort_by) => {
                return Some(false);
            }
            LibraryIntent::SortBy(sort_by) => ItemSortPreferences {
                sort_by,
                ..self.items_sort
            },
            LibraryIntent::SortOrder(sort_order) => ItemSortPreferences {
                sort_order,
                ..self.items_sort
            },
            _ => return None,
        };
        Some(self.set_items_sort(preferences))
    }
}
