//! Home dashboard business flow; presentation and IO executors live outside.
mod controller;
pub(super) mod effect;
mod model;
mod selectors;

#[cfg(test)]
mod tests;

pub(super) use controller::FeedController;
#[cfg(test)]
pub(super) use model::UserViewItemsRow;
pub(super) use model::{FeedIntent, FeedRequest, FeedRequestKind, FeedResponse, FeedUpdate};
#[cfg(test)]
pub(super) use selectors::home_data_section_is_visible;
pub(super) use selectors::{HomeFeedVm, LatestRowVm};
