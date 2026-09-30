use crate::{
    emby::{AuthSession, EmbyClient, ItemCounts, PublicSystemInfo},
    server::{AddServerSubmission, CachedServer},
};
use anyhow::Result;

/// Emby boundary. Domain results only; no GPUI entities, windows or callbacks.
pub(crate) trait ServerGateway: Send + Sync {
    fn public_system_info(&self, submission: &AddServerSubmission) -> Result<PublicSystemInfo>;
    fn authenticate_by_name(&self, submission: &AddServerSubmission) -> Result<AuthSession>;
    fn item_counts(&self, server: &CachedServer) -> Result<ItemCounts>;
    fn matched_icon_url(&self, name: &str) -> Option<String>;
}

impl ServerGateway for EmbyClient {
    fn matched_icon_url(&self, name: &str) -> Option<String> {
        crate::server::icon::match_icon_url(name).map(str::to_owned)
    }
    fn public_system_info(&self, submission: &AddServerSubmission) -> Result<PublicSystemInfo> {
        EmbyClient::public_system_info(self, submission)
    }
    fn authenticate_by_name(&self, submission: &AddServerSubmission) -> Result<AuthSession> {
        EmbyClient::authenticate_by_name(self, submission)
    }
    fn item_counts(&self, server: &CachedServer) -> Result<ItemCounts> {
        EmbyClient::item_counts(self, server)
    }
}
