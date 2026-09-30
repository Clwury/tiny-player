use super::{ImageRepository, controller::ItemImageRequest};
use std::{collections::HashSet, path::PathBuf, sync::Mutex};

#[derive(Debug, Default)]
pub(crate) struct FakeItemImages {
    pub(crate) failed: HashSet<String>,
    pub(crate) requests: Mutex<Vec<ItemImageRequest>>,
}

impl ImageRepository<ItemImageRequest> for FakeItemImages {
    type Image = PathBuf;
    fn load(&self, request: &ItemImageRequest) -> anyhow::Result<PathBuf> {
        self.requests.lock().unwrap().push(request.clone());
        anyhow::ensure!(!self.failed.contains(&request.request.item_id), "offline");
        Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/icons/emby.png"))
    }
}
