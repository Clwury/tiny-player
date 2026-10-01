//! Account-scoped ports supplied by the application composition root.
use std::sync::Arc;

use super::gateway::HomeGateway;
use crate::media::gateway::PlaybackSourceGateway;

#[derive(Clone)]
pub(crate) struct HomePorts {
    pub(super) browsing: Arc<dyn HomeGateway>,
    pub(super) playback: Arc<dyn PlaybackSourceGateway>,
}

impl HomePorts {
    pub(crate) fn new(
        browsing: Arc<dyn HomeGateway>,
        playback: Arc<dyn PlaybackSourceGateway>,
    ) -> Self {
        Self { browsing, playback }
    }
}

impl std::fmt::Debug for HomePorts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HomePorts").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
