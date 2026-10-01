//! Feature API; business policy and GPUI execution have separate owners.
mod binding;
pub(crate) mod controller;
mod effect;
pub(in crate::home) mod model;
mod presentation;
pub(super) mod view;

#[cfg(test)]
mod interaction_tests;

pub(super) use presentation::SearchPresentation;
