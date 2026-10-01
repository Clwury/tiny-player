/// Sidebar projection copied only when the catalog changes. It contains no
/// credentials, endpoint configuration or authentication records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SidebarServer {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) icon_url: Option<String>,
}

impl From<&crate::server::CachedServer> for SidebarServer {
    fn from(server: &crate::server::CachedServer) -> Self {
        Self {
            id: server.id.clone(),
            title: server
                .server_name
                .as_deref()
                .filter(|name| !name.is_empty())
                .unwrap_or(&server.endpoint.address)
                .to_owned(),
            icon_url: server.icon_url.clone(),
        }
    }
}
