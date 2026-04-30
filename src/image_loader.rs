use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::Result;

use crate::{
    emby::EmbyImageRequest,
    image_cache::{self, CachedImageKey},
    server::CachedServer,
};

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
    paths: HashMap<CachedImageKey, PathBuf>,
    queued: VecDeque<ImageLoadJob>,
    queued_keys: HashSet<CachedImageKey>,
    in_flight: HashSet<CachedImageKey>,
    failures: HashMap<CachedImageKey, ImageLoadFailure>,
    max_concurrent: usize,
    retry_after: Duration,
    max_attempts: usize,
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
            paths: HashMap::new(),
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

        if self.paths.contains_key(&key)
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
                self.paths.insert(key, path);
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
                self.paths.insert(key, path);
            }
            Err(error) => self.record_failure(key, error),
        }
    }

    pub(crate) fn path_for_request(
        &self,
        server: &CachedServer,
        request: &EmbyImageRequest,
    ) -> Option<PathBuf> {
        let key = CachedImageKey::from_request(server, request)?;
        self.paths.get(&key).cloned()
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
        if self.paths.contains_key(&key)
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
            access_token: Some("token".to_string()),
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
