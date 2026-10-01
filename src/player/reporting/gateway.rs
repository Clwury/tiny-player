//! Reporting IO port; source selection belongs to media::gateway.
use super::PlaybackReport;

pub(crate) trait PlaybackReportGateway: Send + Sync {
    fn report(&self, report: &PlaybackReport) -> anyhow::Result<()>;
}
