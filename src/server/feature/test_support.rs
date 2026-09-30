//! Fixture access for GPUI regression setup. Production uses immutable selectors
//! and controller intents/results; no mutable catalog escapes that boundary.
use super::*;
use std::collections::{HashMap, HashSet};

pub(crate) struct ServerTestState<'a> {
    pub(crate) counts: &'a HashMap<String, crate::emby::ItemCounts>,
    pub(crate) counts_failed: &'a HashSet<String>,
    pub(crate) counts_refreshed: &'a HashSet<String>,
}

impl ServerController {
    pub(crate) fn test_catalog_mut(&mut self) -> &mut ServerCatalog {
        &mut self.catalog
    }

    pub(crate) fn test_state(&self) -> ServerTestState<'_> {
        ServerTestState {
            counts: &self.counts,
            counts_failed: &self.counts_failed,
            counts_refreshed: &self.counts_refreshed,
        }
    }
}
