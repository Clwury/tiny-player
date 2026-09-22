use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;

use crate::{
    emby::{EmbyImageRequest, EmbyImageType, ImageQuality},
    server::CachedServer,
};

use super::cache::{self as image_cache, CachedImageKey};

const DEFAULT_MAX_CONCURRENT_IMAGES: usize = 100;
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(30);
const DEFAULT_MAX_ATTEMPTS: usize = 3;

#[derive(Clone, Debug)]
pub(crate) struct ImageLoadJob {
    pub(crate) key: CachedImageKey,
    pub(crate) request: EmbyImageRequest,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) struct ImageLoadFailure {
    pub(crate) message: String,
    attempts: usize,
    failed_at: Instant,
}

#[derive(Clone, Debug)]
pub(crate) struct ImageLoader {
    // Image paths are shared with every card that references them. The nested,
    // borrowed-friendly index both returns an `Arc<Path>` (avoiding path clones)
    // and avoids constructing an owned lookup key on every render. Constructing a
    // `CachedImageKey` for every visible card would clone the item id, server id
    // and image tag on every frame (window resizing makes that especially
    // noticeable). Queue/in-flight sets still own `CachedImageKey`s; this index
    // is the source of truth for completed paths.
    paths_by_server: HashMap<String, HashMap<String, Vec<ImagePathEntry>>>,
    queued: VecDeque<ImageLoadJob>,
    queued_keys: HashSet<CachedImageKey>,
    in_flight: HashSet<CachedImageKey>,
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

impl Default for ImageLoader {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageLoader {
    pub(crate) fn new() -> Self {
        Self::with_limits(
            DEFAULT_MAX_CONCURRENT_IMAGES,
            DEFAULT_RETRY_AFTER,
            DEFAULT_MAX_ATTEMPTS,
        )
    }

    pub(crate) fn with_limits(
        max_concurrent: usize,
        retry_after: Duration,
        max_attempts: usize,
    ) -> Self {
        Self {
            paths_by_server: HashMap::new(),
            queued: VecDeque::new(),
            queued_keys: HashSet::new(),
            in_flight: HashSet::new(),
            failures: HashMap::new(),
            max_concurrent: max_concurrent.max(1),
            retry_after,
            max_attempts: max_attempts.max(1),
        }
    }

    pub(crate) fn ensure_image(&mut self, server: &CachedServer, request: EmbyImageRequest) {
        let Some(key) = CachedImageKey::from_request(server, &request) else {
            return;
        };

        if self.path_for_key(&key).is_some()
            || self.queued_keys.contains(&key)
            || self.in_flight.contains(&key)
        {
            return;
        }

        if !self.failure_can_retry(&key) {
            return;
        }

        match image_cache::cached_image_exists(&key) {
            Ok(Some(path)) => {
                self.failures.remove(&key);
                self.insert_path(key, Arc::from(path));
            }
            Ok(None) => self.queue_job(key, request),
            Err(error) => self.record_failure(key, error),
        }
    }

    pub(crate) fn start_queued_jobs(&mut self) -> Vec<ImageLoadJob> {
        let available = self.max_concurrent.saturating_sub(self.in_flight.len());
        let mut jobs = Vec::with_capacity(available);

        for _ in 0..available {
            let Some(job) = self.queued.pop_front() else {
                break;
            };
            self.queued_keys.remove(&job.key);
            self.in_flight.insert(job.key.clone());
            jobs.push(job);
        }

        jobs
    }

    pub(crate) fn finish_job(&mut self, key: CachedImageKey, result: Result<PathBuf>) {
        self.in_flight.remove(&key);

        match result {
            Ok(path) => {
                self.failures.remove(&key);
                self.insert_path(key, Arc::from(path));
            }
            Err(error) => self.record_failure(key, error),
        }
    }

    pub(crate) fn path_for_request(
        &self,
        server: &CachedServer,
        request: &EmbyImageRequest,
    ) -> Option<Arc<Path>> {
        self.path_for_source(
            server,
            request.item_id.as_str(),
            request.image_type,
            request.tag.as_deref(),
            request.max_width,
            request.quality,
        )
    }

    pub(crate) fn path_for_source(
        &self,
        server: &CachedServer,
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
        let server_paths = self.paths_by_server.get(server.id.as_str())?;
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
        server: &CachedServer,
        request: &EmbyImageRequest,
    ) -> Option<&ImageLoadFailure> {
        let key = CachedImageKey::from_request(server, request)?;
        self.failures.get(&key)
    }

    fn queue_job(&mut self, key: CachedImageKey, request: EmbyImageRequest) {
        if self.path_for_key(&key).is_some()
            || self.queued_keys.contains(&key)
            || self.in_flight.contains(&key)
        {
            return;
        }

        self.queued_keys.insert(key.clone());
        self.queued.push_back(ImageLoadJob { key, request });
    }

    fn failure_can_retry(&mut self, key: &CachedImageKey) -> bool {
        let Some(failure) = self.failures.get(key) else {
            return true;
        };

        if failure.attempts >= self.max_attempts {
            return false;
        }

        failure.failed_at.elapsed() >= self.retry_after
    }

    fn record_failure(&mut self, key: CachedImageKey, error: anyhow::Error) {
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
                failed_at: Instant::now(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use anyhow::anyhow;

    use crate::{
        emby::{EmbyImageType, ImageQuality},
        server::{CachedServer, Protocol, ServerEndpoint},
    };

    use super::*;

    fn server() -> CachedServer {
        CachedServer {
            id: "server-loader-test".to_string(),
            endpoint: ServerEndpoint {
                protocol: Protocol::Https,
                address: "example.com".to_string(),
                port: 443,
                path: "/emby".to_string(),
            },
            username: "luv".to_string(),
            password: "secret".to_string(),
            user_id: Some("user-1".to_string()),
            server_id: Some("server-1".to_string()),
            server_name: Some("Home".to_string()),
            icon_url: None,
            icon_is_custom: false,
            access_token: Some("token".to_string()),
            needs_auth_refresh: false,
            item_counts: None,
            added_at_unix: 123,
        }
    }

    fn request(item_id: &str) -> EmbyImageRequest {
        EmbyImageRequest::new(item_id, EmbyImageType::Primary)
            .with_tag(Some(format!("tag-{item_id}")))
            .with_quality(ImageQuality::DEFAULT)
    }

    #[test]
    fn dedupes_queued_and_in_flight_jobs() {
        let server = server();
        let request = request("dedupe-1");
        let mut loader = ImageLoader::with_limits(2, Duration::ZERO, 3);

        loader.ensure_image(&server, request.clone());
        loader.ensure_image(&server, request.clone());

        let jobs = loader.start_queued_jobs();
        assert_eq!(jobs.len(), 1);
        assert!(loader.start_queued_jobs().is_empty());

        loader.ensure_image(&server, request);
        assert!(loader.start_queued_jobs().is_empty());
    }

    #[test]
    fn completed_path_lookup_does_not_require_rebuilding_the_owned_key() {
        let server = server();
        let lookup_request = request("lookup-1");
        let mut loader = ImageLoader::with_limits(1, Duration::ZERO, 1);

        loader.ensure_image(&server, lookup_request.clone());
        let job = loader.start_queued_jobs().pop().unwrap();
        loader.finish_job(job.key, Ok(PathBuf::from("/tmp/lookup-1.jpg")));

        assert_eq!(
            loader.path_for_request(&server, &lookup_request),
            Some(Arc::from(Path::new("/tmp/lookup-1.jpg")))
        );
        assert!(
            loader
                .path_for_request(&server, &request("other"))
                .is_none()
        );
    }

    #[test]
    fn respects_concurrency_limit_and_starts_next_after_finish() {
        let server = server();
        let mut loader = ImageLoader::with_limits(2, Duration::ZERO, 3);
        for id in ["limit-1", "limit-2", "limit-3"] {
            loader.ensure_image(&server, request(id));
        }

        let jobs = loader.start_queued_jobs();
        assert_eq!(jobs.len(), 2);

        let first_key = jobs[0].key.clone();
        loader.finish_job(first_key, Err(anyhow!("network failed")));

        let next = loader.start_queued_jobs();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].request.item_id, "limit-3");
    }

    #[test]
    fn records_failures_and_allows_delayed_retry() {
        let server = server();
        let request = request("retry-1");
        let mut loader = ImageLoader::with_limits(1, Duration::ZERO, 3);

        loader.ensure_image(&server, request.clone());
        let job = loader.start_queued_jobs().pop().unwrap();
        loader.finish_job(job.key, Err(anyhow!("temporary error")));

        let failure = loader.failure_for_request(&server, &request).unwrap();
        assert_eq!(failure.attempts, 1);
        assert_eq!(failure.message, "temporary error");

        loader.ensure_image(&server, request);
        assert_eq!(loader.start_queued_jobs().len(), 1);
    }
}
