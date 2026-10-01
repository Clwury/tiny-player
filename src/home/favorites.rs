//! Feature API; business policy and GPUI execution have separate owners.
pub(crate) mod actions;
mod binding;
pub(super) mod controller;
mod effect;
mod presentation;
pub(super) mod view;

#[cfg(test)]
mod integration_tests;

pub(crate) use controller::{
    FAVORITE_ITEM_TYPES, FAVORITES_PAGE_LIMIT, FavoritesController, favorite_section_title,
};
pub(crate) use presentation::FavoritesPresentation;
