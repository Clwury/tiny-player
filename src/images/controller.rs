use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;

use crate::{
    effects::{RequestScope, RequestSlot, RequestToken, WorkspaceIdentity},
    emby::{EmbyImageRequest, EmbyImageType, ImageQuality},
};

use super::cache::CachedImageKey;

const DEFAULT_MAX_CONCURRENT_IMAGES: usize = 100;
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(30);
const DEFAULT_MAX_ATTEMPTS: usize = 3;

#[derive(Clone, Debug)]
pub(crate) struct ItemImageRequest {
    pub(crate) key: CachedImageKey,
    pub(crate) request: EmbyImageRequest,
}

#[derive(Clone, Debug)]
pub(crate) struct ItemImageCommand {
    pub(crate) image: ItemImageRequest,
    pub(crate) token: RequestToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImageUpdate {
    Ignored,
    Failed,
    Ready,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) struct ImageLoadFailure {
    pub(crate) message: String,
    attempts: usize,
    failed_at: Instant,
}

/// Workspace-scoped presentation data and scheduler. Ensure/complete are the
/// only write paths; completed paths survive route changes for warm-cache reuse.
/// The runner owns cancellation handles; dropping this owner invalidates slots.
#[derive(Debug)]
pub(crate) struct ImageController {
    // Image paths are shared with every card that references them. The nested,
    // borrowed-friendly index both returns an `Arc<Path>` (avoiding path clones)
    // and avoids constructing an owned lookup key on every render. Constructing a
    // `CachedImageKey` for every visible card would clone the item id, server id
    // and image tag on every frame (window resizing makes that especially
    // noticeable). Queue/in-flight sets still own `CachedImageKey`s; this index
    // is the source of truth for completed paths.
    identity: WorkspaceIdentity,
    paths_by_server: HashMap<String, HashMap<String, Vec<ImagePathEntry>>>,
    queued: VecDeque<ItemImageRequest>,
    queued_keys: HashSet<CachedImageKey>,
    in_flight: HashMap<CachedImageKey, RequestSlot>,
    failures: HashMap<CachedImageKey, ImageLoadFailure>,
    max_concurrent: usize,
    retry_after: Duration,
    max_attempts: usize,
}

#[derive(Clone, Debug)]
struct ImagePathEntry {
    image_type: EmbyImageType,
    tag: String,
    max_width: Option<u32>,
    quality: ImageQuality,
    path: Arc<Path>,
}

impl ImagePathEntry {
    fn matches_key(&self, key: &CachedImageKey) -> bool {
        self.image_type == key.image_type
            && self.tag == key.tag
            && self.max_width == key.max_width
            && self.quality == key.quality
    }
}

impl ImageController {
    pub(crate) fn new(identity: WorkspaceIdentity) -> Self {
        Self::with_limits(
            identity,
            DEFAULT_MAX_CONCURRENT_IMAGES,
            DEFAULT_RETRY_AFTER,
            DEFAULT_MAX_ATTEMPTS,
        )
    }

    pub(crate) fn with_limits(
        identity: WorkspaceIdentity,
        max_concurrent: usize,
        retry_after: Duration,
        max_attempts: usize,
    ) -> Self {
        Self {
            identity,
            paths_by_server: HashMap::new(),
            queued: VecDeque::new(),
            queued_keys: HashSet::new(),
            in_flight: HashMap::new(),
            failures: HashMap::new(),
            max_concurrent: max_concurrent.max(1),
            retry_after,
            max_attempts: max_attempts.max(1),
        }
    }

    pub(crate) fn ensure_image(&mut self, request: EmbyImageRequest, now: Instant) {
        let Some(key) = CachedImageKey::from_request(&self.identity.local_server_id, &request)
        else {
            return;
        };

        if self.path_for_key(&key).is_some()
            || self.queued_keys.contains(&key)
            || self.in_flight.contains_key(&key)
        {
            return;
        }

        if !self.failure_can_retry(&key, now) {
            return;
        }

        self.queue_job(key, request);
    }

    pub(crate) fn start_queued_jobs(&mut self) -> Vec<ItemImageCommand> {
        let available = self.max_concurrent.saturating_sub(self.in_flight.len());
        let mut jobs = Vec::with_capacity(available);
        for _ in 0..available {
            let Some(image) = self.queued.pop_front() else {
                break;
            };
            self.queued_keys.remove(&image.key);
            let mut slot = RequestSlot::new(
                RequestScope::ItemImage {
                    key: image.key.clone(),
                },
                self.identity.clone(),
            );
            let token = slot.issue();
            self.in_flight.insert(image.key.clone(), slot);
            jobs.push(ItemImageCommand { image, token });
        }
        jobs
    }

    pub(crate) fn finish_job(
        &mut self,
        command: &ItemImageCommand,
        result: Result<PathBuf>,
        identity: &WorkspaceIdentity,
        now: Instant,
    ) -> ImageUpdate {
        if !command.token.is_for(identity)
            || !self
                .in_flight
                .get_mut(&command.image.key)
                .is_some_and(|slot| slot.commit(&command.token))
        {
            return ImageUpdate::Ignored;
        }
        let key = command.image.key.clone();
        self.in_flight.remove(&key);
        match result {
            Ok(path) => {
                self.failures.remove(&key);
                self.insert_path(key, Arc::from(path));
                ImageUpdate::Ready
            }
            Err(error) => {
                self.record_failure(key, error, now);
                ImageUpdate::Failed
            }
        }
    }

    pub(crate) fn path_for_request(&self, request: &EmbyImageRequest) -> Option<Arc<Path>> {
        self.path_for_source(
            request.item_id.as_str(),
            request.image_type,
            request.tag.as_deref(),
            request.max_width,
            request.quality,
        )
    }

    pub(crate) fn path_for_source(
        &self,
        item_id: &str,
        image_type: EmbyImageType,
        tag: Option<&str>,
        max_width: Option<u32>,
        quality: ImageQuality,
    ) -> Option<Arc<Path>> {
        let tag = tag?.trim();
        if tag.is_empty() {
            return None;
        }
        let server_paths = self
            .paths_by_server
            .get(self.identity.local_server_id.as_str())?;
        let item_paths = server_paths.get(item_id)?;
        item_paths
            .iter()
            .find(|entry| {
                entry.image_type == image_type
                    && entry.tag == tag
                    && entry.max_width == max_width
                    && entry.quality == quality
            })
            .map(|entry| entry.path.clone())
    }

    #[cfg(test)]
    pub(crate) fn test_install_path(&mut self, key: CachedImageKey, path: PathBuf) {
        self.insert_path(key, Arc::from(path));
    }

    fn insert_path(&mut self, key: CachedImageKey, path: Arc<Path>) {
        let server_paths = self
            .paths_by_server
            .entry(key.server_id.clone())
            .or_default();
        let item_paths = server_paths.entry(key.item_id.clone()).or_default();
        if let Some(entry) = item_paths.iter_mut().find(|entry| entry.matches_key(&key)) {
            entry.path = path;
        } else {
            item_paths.push(ImagePathEntry {
                image_type: key.image_type,
                tag: key.tag,
                max_width: key.max_width,
                quality: key.quality,
                path,
            });
        }
    }

    fn path_for_key(&self, key: &CachedImageKey) -> Option<&Arc<Path>> {
        self.paths_by_server
            .get(key.server_id.as_str())?
            .get(key.item_id.as_str())?
            .iter()
            .find(|entry| entry.matches_key(key))
            .map(|entry| &entry.path)
    }

    #[allow(dead_code)]
    pub(crate) fn failure_for_request(
        &self,
        request: &EmbyImageRequest,
    ) -> Option<&ImageLoadFailure> {
        let key = CachedImageKey::from_request(&self.identity.local_server_id, request)?;
        self.failures.get(&key)
    }

    fn queue_job(&mut self, key: CachedImageKey, request: EmbyImageRequest) {
        if self.path_for_key(&key).is_some()
            || self.queued_keys.contains(&key)
            || self.in_flight.contains_key(&key)
        {
            return;
        }

        self.queued_keys.insert(key.clone());
        self.queued.push_back(ItemImageRequest { key, request });
    }

    fn failure_can_retry(&self, key: &CachedImageKey, now: Instant) -> bool {
        let Some(failure) = self.failures.get(key) else {
            return true;
        };

        if failure.attempts >= self.max_attempts {
            return false;
        }

        now.saturating_duration_since(failure.failed_at) >= self.retry_after
    }

    fn record_failure(&mut self, key: CachedImageKey, error: anyhow::Error, now: Instant) {
        let attempts = self
            .failures
            .get(&key)
            .map(|failure| failure.attempts.saturating_add(1))
            .unwrap_or(1);

        self.failures.insert(
            key,
            ImageLoadFailure {
                message: error.to_string(),
                attempts,
                failed_at: now,
            },
        );
    }
}

#[cfg(test)]
#[path = "controller_tests.rs"]
mod tests;
