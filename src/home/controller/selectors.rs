use super::HomeController;
use crate::emby::{ResumeItem, UserItem, UserItemData};
use crate::home::model::navigation::{HomeRoot, HomeRoute};
use std::borrow::Cow;

impl HomeController {
    pub(in crate::home) fn user_item_by_id(&self, item_id: &str) -> Option<UserItem> {
        let current_route_item = match self.navigation.current() {
            HomeRoute::Root(HomeRoot::Favorites) => self
                .favorites
                .overview_items()
                .find(|item| item.id == item_id),
            HomeRoute::FavoriteItems { item_type } => self
                .favorites
                .view_model(*item_type)
                .paged
                .items
                .iter()
                .find(|item| item.id == item_id),
            HomeRoute::Root(HomeRoot::Search) => self
                .search
                .view_model()
                .items
                .iter()
                .find(|item| item.id == item_id),
            HomeRoute::Library { view_id, .. } => self.libraries.get(view_id).and_then(|library| {
                library
                    .view_model()
                    .paged
                    .items
                    .iter()
                    .find(|item| item.id == item_id)
            }),
            HomeRoute::Person { person_id, .. } => self.persons.get(person_id).and_then(|person| {
                person
                    .items
                    .view_model()
                    .paged
                    .items
                    .iter()
                    .find(|item| item.id == item_id)
            }),
            HomeRoute::Genre { genre_key, .. } => self.genres.get(genre_key).and_then(|genre| {
                genre
                    .view_model()
                    .paged
                    .items
                    .iter()
                    .find(|item| item.id == item_id)
            }),
            HomeRoute::Detail { .. } => self
                .navigation
                .detail()
                .and_then(|detail| detail.view_model().similar_items.as_ref())
                .and_then(|items| items.items.iter().find(|item| item.id == item_id)),
            HomeRoute::Root(HomeRoot::Home) => None,
        };

        let item = current_route_item
            .or_else(|| {
                self.feed
                    .state
                    .user_view_items_rows
                    .values()
                    .filter_map(|row| row.items.as_ref())
                    .flat_map(|items| items.items.iter())
                    .find(|item| item.id == item_id)
            })
            .or_else(|| self.favorites.items().find(|item| item.id == item_id))
            .or_else(|| {
                self.search
                    .view_model()
                    .items
                    .iter()
                    .find(|item| item.id == item_id)
            })
            .or_else(|| {
                self.libraries
                    .values()
                    .chain(self.genres.values())
                    .flat_map(|library| library.view_model().paged.items.iter())
                    .find(|item| item.id == item_id)
            })
            .or_else(|| {
                self.persons
                    .values()
                    .flat_map(|person| person.items.view_model().paged.items.iter())
                    .find(|item| item.id == item_id)
            })?;

        Some(self.effective_user_item(item).into_owned())
    }

    pub(in crate::home) fn resume_item_by_id(&self, item_id: &str) -> Option<ResumeItem> {
        let item = self
            .feed
            .state
            .resume_items
            .as_ref()?
            .items
            .iter()
            .find(|item| item.id == item_id)?;
        Some(self.effective_resume_item(item).into_owned())
    }

    pub(in crate::home) fn loaded_playback_user_data(
        &self,
        item_id: &str,
    ) -> Option<&UserItemData> {
        if let Some(data) = self.user_data.overrides.get(item_id) {
            return Some(data);
        }
        if let Some(data) = self
            .navigation
            .detail()
            .and_then(|detail| detail.view_model().playback_user_data(item_id))
        {
            return Some(data);
        }
        if let Some(data) = self
            .feed
            .state
            .resume_items
            .as_ref()
            .and_then(|items| items.items.iter().find(|item| item.id == item_id))
            .and_then(|item| item.user_data.as_ref())
        {
            return Some(data);
        }
        for row in self.feed.state.user_view_items_rows.values() {
            if let Some(data) = row
                .items
                .as_ref()
                .and_then(|items| items.items.iter().find(|item| item.id == item_id))
                .and_then(|item| item.user_data.as_ref())
            {
                return Some(data);
            }
        }
        for library in self.libraries.values().chain(self.genres.values()) {
            if let Some(data) = library
                .view_model()
                .paged
                .items
                .iter()
                .find(|item| item.id == item_id)
                .and_then(|item| item.user_data.as_ref())
            {
                return Some(data);
            }
        }
        for person in self.persons.values() {
            let vm = person.view_model();
            if let Some(data) = vm
                .items
                .paged
                .items
                .iter()
                .find(|item| item.id == item_id)
                .and_then(|item| item.user_data.as_ref())
            {
                return Some(data);
            }
        }
        self.favorites
            .items()
            .find(|item| item.id == item_id)
            .and_then(|item| item.user_data.as_ref())
            .or_else(|| {
                self.search
                    .view_model()
                    .items
                    .iter()
                    .find(|item| item.id == item_id)
                    .and_then(|item| item.user_data.as_ref())
            })
    }

    pub(in crate::home) fn user_data_request_revision(&self) -> u64 {
        self.user_data.revision
    }

    pub(in crate::home) fn effective_user_data<'a>(
        &'a self,
        item_id: &str,
        fallback: Option<&'a UserItemData>,
    ) -> Option<&'a UserItemData> {
        self.user_data.effective(item_id, fallback)
    }

    pub(in crate::home) fn effective_user_item<'a>(&self, item: &'a UserItem) -> Cow<'a, UserItem> {
        self.user_data.item(item)
    }

    pub(in crate::home) fn effective_resume_item<'a>(
        &self,
        item: &'a ResumeItem,
    ) -> Cow<'a, ResumeItem> {
        self.user_data.resume(item)
    }
}
