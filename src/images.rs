pub(crate) mod cache;
pub(crate) mod controller;
pub(crate) mod cover;
pub(crate) mod item_images;
pub(crate) mod server_icon_assets;
pub(crate) mod server_icons;
#[cfg(test)]
pub(crate) mod test_support;

/// Image IO boundary. RenderImage remains a presentation payload; no window,
/// entity or context crosses this port. Display ownership stays in presentation
/// adapters; shared cover assets retain the GPUI application-cache lifetime.
pub(crate) trait ImageRepository<Request>: Send + Sync + std::fmt::Debug {
    type Image;
    fn load(&self, request: &Request) -> anyhow::Result<Self::Image>;
}

pub(crate) struct ServerIconRequest {
    pub(crate) url: String,
    pub(crate) load: ServerIconLoad,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServerIconLoad {
    Cached,
    Preview,
}

#[derive(Debug)]
pub(crate) struct FileImageRepository;

impl ImageRepository<ServerIconRequest> for FileImageRepository {
    type Image = std::sync::Arc<gpui::RenderImage>;
    fn load(&self, request: &ServerIconRequest) -> anyhow::Result<Self::Image> {
        match request.load {
            ServerIconLoad::Cached => server_icons::cache_server_icon(&request.url),
            ServerIconLoad::Preview => server_icons::load_icon(&request.url),
        }
    }
}
