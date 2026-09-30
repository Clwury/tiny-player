//! Queue policy and source-resolution effect, independent of GPUI execution.
mod controller;
pub(super) mod effect;
mod model;
pub(super) use controller::QueueController;
pub(super) use model::{QueueAction, QueueSwitchCommand, QueueSwitchUpdate, ResolvedQueuePlayback};

#[cfg(test)]
mod controller_tests;
#[cfg(test)]
mod effect_tests;
#[cfg(test)]
mod test_support;
