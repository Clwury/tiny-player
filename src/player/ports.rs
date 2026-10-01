//! Account-scoped playback IO supplied by the application composition root.
use std::sync::Arc;

use super::reporting::gateway::PlaybackReportGateway;
use crate::media::gateway::PlaybackSourceGateway;

#[derive(Clone)]
pub(crate) struct PlaybackPorts {
    pub(super) source: Arc<dyn PlaybackSourceGateway>,
    pub(super) reporting: Arc<dyn PlaybackReportGateway>,
}

impl PlaybackPorts {
    pub(crate) fn new(
        source: Arc<dyn PlaybackSourceGateway>,
        reporting: Arc<dyn PlaybackReportGateway>,
    ) -> Self {
        Self { source, reporting }
    }
}
