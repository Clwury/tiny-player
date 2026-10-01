//! Playback reporting policy and effects; independent of page/backend ownership.
mod completion;
mod controller;
pub(super) mod effect;
pub(crate) mod gateway;

pub use completion::{PlaybackStateUpdate, PlaybackStopCompletion, PlaybackStopResult};
pub(crate) use controller::PlaybackReport;
pub(super) use controller::{
    ReportContext, ReportTelemetry, ReportingCommand, ReportingController, ReportingIntent,
    ReportingTransition,
};

pub(super) const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(test)]
mod controller_tests;
#[cfg(test)]
pub(in crate::player) mod test_support;
