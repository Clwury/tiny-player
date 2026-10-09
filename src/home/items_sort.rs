//! Synchronize the application-wide sort with workspace requests and resources.
use crate::{
    home::{
        HomeContent,
        favorites::FAVORITE_ITEM_TYPES,
        library::{
            ItemsSortTarget,
            controller::{LibraryIntent, LibrarySource},
        },
        model::navigation::{HomeRoot, HomeRoute},
    },
    media::ItemSortPreferences,
};
use gpui::{Context, point, px};

impl HomeContent {
    pub(in crate::home) fn apply_shared_items_sort(
        &mut self,
        preferences: ItemSortPreferences,
        cx: &mut Context<Self>,
    ) {
        if !self.controller.set_items_sort(preferences) {
            return;
        }
        for resources in self
            .library_resources
            .values_mut()
            .chain(self.genre_resources.values_mut())
        {
            resources.effect.cancel();
            resources.presentation.sort_menu_open = false;
            resources
                .presentation
                .grid
                .scroll_handle
                .set_offset(point(px(0.0), px(0.0)));
        }
        for resources in self.person_resources.values_mut() {
            resources.items.effect.cancel();
            resources.items.presentation.sort_menu_open = false;
            resources
                .items
                .presentation
                .grid
                .scroll_handle
                .set_offset(point(px(0.0), px(0.0)));
        }
        for item_type in FAVORITE_ITEM_TYPES {
            let section = &mut self.favorites_presentation[item_type];
            section.effect.cancel();
            section.sort_menu_open = false;
            section
                .presentation
                .scroll_handle
                .set_offset(point(px(0.0), px(0.0)));
        }
        self.item_context_menu = None;
        self.layout.content_changed();
        if self.controller.route() != &HomeRoute::Root(HomeRoot::Favorites) {
            self.enter_current_items_if_needed(cx);
        }
        cx.notify();
    }

    pub(in crate::home) fn enter_current_items_if_needed(&mut self, cx: &mut Context<Self>) {
        if self.authentication_error.is_some() {
            return;
        }
        match self.controller.route().clone() {
            HomeRoute::Library {
                view_id,
                item_types,
                ..
            } => self.dispatch_items_source(
                &LibrarySource::View(view_id),
                LibraryIntent::Open(item_types),
                cx,
            ),
            HomeRoute::Person { .. } => self.enter_current_person_if_needed(cx),
            HomeRoute::Genre { genre_key, .. } => self.dispatch_items_source(
                &LibrarySource::Genre(genre_key),
                LibraryIntent::Open(vec![
                    crate::emby::VideoItemType::Movie,
                    crate::emby::VideoItemType::Series,
                ]),
                cx,
            ),
            HomeRoute::Root(HomeRoot::Favorites) | HomeRoute::FavoriteItems { .. } => {
                self.enter_favorites_if_needed(cx)
            }
            _ => {}
        }
    }

    pub(in crate::home) fn items_sort_target_is_current(&self, target: &ItemsSortTarget) -> bool {
        match (target, self.controller.route()) {
            (
                ItemsSortTarget::Library(LibrarySource::View(id)),
                HomeRoute::Library { view_id, .. },
            ) => id == view_id,
            (
                ItemsSortTarget::Library(LibrarySource::Person(id)),
                HomeRoute::Person { person_id, .. },
            ) => id == person_id,
            (
                ItemsSortTarget::Library(LibrarySource::Genre(key)),
                HomeRoute::Genre { genre_key, .. },
            ) => key == genre_key,
            (ItemsSortTarget::Favorites(kind), HomeRoute::FavoriteItems { item_type }) => {
                kind == item_type
            }
            _ => false,
        }
    }
}
