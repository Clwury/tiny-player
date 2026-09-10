use std::{future::Future, pin::pin, sync::Arc, time::Duration};

use reqwest::{RequestBuilder, Response};
use tokio::runtime::Runtime;

use super::{
    CacheRestartRequest, HTTP_CACHE_NETWORK_READ_TIMEOUT, HttpDownloadError, HttpRingCacheShared,
    http_cache_request_should_retry,
};

/// Keep the demux/cache workers synchronous, but poll network I/O with a
/// cancellable future. Dropping the future/response closes an obsolete request;
/// it does not inject an error into a cache-hit AVIO seek.
pub(super) struct HttpClient {
    pub(super) inner: reqwest::Client,
    runtime: Runtime,
}

impl HttpClient {
    pub(super) fn new() -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let inner = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self { inner, runtime })
    }

    pub(super) fn send(
        &self,
        request: RequestBuilder,
        shared: Arc<HttpRingCacheShared>,
        side: Option<CacheRestartRequest>,
        offset: u64,
        generation: u64,
    ) -> Result<HttpResponse<'_>, HttpDownloadError> {
        let future = {
            // reqwest constructs a timer eagerly for request-level timeouts.
            let _entered = self.runtime.enter();
            request.send()
        };
        let response = self.run(&shared, generation, side, offset, future)?;
        if side.is_none() {
            shared.set_download_active(generation, true);
        }
        Ok(HttpResponse {
            response,
            client: self,
            shared,
            generation,
            side,
            pending: Vec::new(),
            pending_pos: 0,
        })
    }

    fn run<T>(
        &self,
        shared: &HttpRingCacheShared,
        generation: u64,
        side: Option<CacheRestartRequest>,
        offset: u64,
        future: impl Future<Output = Result<T, reqwest::Error>>,
    ) -> Result<T, HttpDownloadError> {
        self.runtime.block_on(async {
            let mut future = pin!(future);
            let deadline = tokio::time::Instant::now() + HTTP_CACHE_NETWORK_READ_TIMEOUT;
            loop {
                if shared.request_cancelled(generation, side) {
                    return Err(HttpDownloadError::cancelled(offset));
                }
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    return Err(HttpDownloadError::new(
                        offset,
                        "HTTP 视频缓存网络读取超时".into(),
                        true,
                    ));
                }
                let poll_until = deadline.min(now + Duration::from_millis(25));
                if let Ok(result) = tokio::time::timeout_at(poll_until, &mut future).await {
                    // A seek can race with a ready response. Never publish its
                    // headers or bytes into the replacement request's state.
                    if shared.request_cancelled(generation, side) {
                        return Err(HttpDownloadError::cancelled(offset));
                    }
                    return result.map_err(|error| {
                        let retryable = http_cache_request_should_retry(&error);
                        HttpDownloadError::new(
                            offset,
                            format!("HTTP 视频缓存网络请求失败：{}", error.without_url()),
                            retryable,
                        )
                    });
                }
            }
        })
    }
}

pub(super) struct HttpResponse<'a> {
    pub(super) response: Response,
    client: &'a HttpClient,
    shared: Arc<HttpRingCacheShared>,
    generation: u64,
    side: Option<CacheRestartRequest>,
    pending: Vec<u8>,
    pending_pos: usize,
}

impl HttpResponse<'_> {
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }
    pub(super) fn read(
        &mut self,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, HttpDownloadError> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.shared.request_cancelled(self.generation, self.side) {
            return Err(HttpDownloadError::cancelled(offset));
        }
        while self.pending_pos == self.pending.len() {
            let chunk = self.client.run(
                &self.shared,
                self.generation,
                self.side,
                offset,
                self.response.chunk(),
            )?;
            let Some(chunk) = chunk else { return Ok(0) };
            self.pending.clear();
            self.pending.extend_from_slice(&chunk);
            self.pending_pos = 0;
        }
        let read = output.len().min(self.pending.len() - self.pending_pos);
        output[..read].copy_from_slice(&self.pending[self.pending_pos..self.pending_pos + read]);
        self.pending_pos += read;
        Ok(read)
    }
}

impl Drop for HttpResponse<'_> {
    fn drop(&mut self) {
        if self.side.is_none() {
            self.shared.set_download_active(self.generation, false);
        }
    }
}
