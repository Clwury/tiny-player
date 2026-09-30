//! Native resource adapter owned by the page. Controller code sees only domain
//! events/results; callers cannot take ownership of decoder or presenter.
mod lifetime;
mod native;

use lifetime::ShutdownOrder;
use tiny_playback::{
    BackendCommand, BackendEvent, BackendLoadRequest, BgraImage, RenderSize, Result,
    VideoPresenterSnapshot,
};

/// Driver events have already passed the engine's worker-session validation.
/// A page gets a fresh driver for each source; replacement drops the old owner.
pub(super) trait PlaybackDriver {
    fn command(&mut self, command: BackendCommand) -> Result<()>;
    fn poll_events(&mut self) -> Vec<BackendEvent>;
    fn create_presenter(&self) -> anyhow::Result<Box<dyn FramePresenter>>;
}

pub(super) trait FramePresenter {
    fn prewarm(&mut self);
    fn render(&mut self, size: RenderSize) -> anyhow::Result<Option<BgraImage>>;
    fn discard_pending(&mut self);
    fn snapshot(&self) -> VideoPresenterSnapshot;
}

// One page owns the adapter from construction through replacement/release.
// Only this module writes native resources; Drop releases presenter before driver.
pub(super) struct PlaybackBackendAdapter {
    resources: ShutdownOrder<Box<dyn PlaybackDriver>, Box<dyn FramePresenter>>,
}

impl PlaybackBackendAdapter {
    pub(super) fn start(request: BackendLoadRequest, volume: f32) -> (Self, Option<String>) {
        Self::start_with(
            || {
                Ok(Box::new(native::PlaybackBackend::Ffmpeg(
                    tiny_playback::FfmpegBackend::new()?,
                )))
            },
            request,
            volume,
        )
    }

    fn start_with(
        factory: impl FnOnce() -> Result<Box<dyn PlaybackDriver>>,
        mut request: BackendLoadRequest,
        volume: f32,
    ) -> (Self, Option<String>) {
        let mut backend = match factory() {
            Ok(backend) => backend,
            Err(error) => {
                return (
                    Self::from_resources(None, None),
                    Some(format!("创建 FFmpeg 播放后端失败：{error}")),
                );
            }
        };
        let presenter = match backend.create_presenter() {
            Ok(presenter) => presenter,
            Err(error) => {
                return (
                    Self::from_resources(Some(backend), None),
                    Some(format!("创建视频渲染器失败：{error}")),
                );
            }
        };
        request.cache_config = super::cache::engine_cache_config(request.cache_config).normalized();
        // Restore mute/volume before the worker can produce its first samples.
        let error = backend
            .command(BackendCommand::SetVolume { volume })
            .and_then(|()| backend.command(BackendCommand::Load(request)))
            .err()
            .map(|error| format!("加载视频失败：{error}"));
        (Self::from_resources(Some(backend), Some(presenter)), error)
    }

    fn from_resources(
        backend: Option<Box<dyn PlaybackDriver>>,
        presenter: Option<Box<dyn FramePresenter>>,
    ) -> Self {
        Self {
            resources: ShutdownOrder::new(backend, presenter),
        }
    }

    pub(super) fn has_backend(&self) -> bool {
        self.resources.owner().is_some()
    }
    pub(super) fn has_presenter(&self) -> bool {
        self.resources.dependent().is_some()
    }
    pub(super) fn command(&mut self, command: BackendCommand) -> Option<Result<()>> {
        let command = match command {
            BackendCommand::Load(mut request) => {
                request.cache_config = super::cache::engine_cache_config(request.cache_config);
                BackendCommand::Load(request)
            }
            BackendCommand::SetCacheConfig(config) => {
                BackendCommand::SetCacheConfig(super::cache::engine_cache_config(config))
            }
            command => command,
        };
        self.resources
            .owner_mut()
            .map(|backend| backend.command(command))
    }
    pub(super) fn poll_events(&mut self) -> Vec<BackendEvent> {
        self.resources
            .owner_mut()
            .map(|backend| backend.poll_events())
            .unwrap_or_default()
    }
    pub(super) fn prewarm(&mut self) {
        if let Some(presenter) = self.resources.dependent_mut() {
            presenter.prewarm();
        }
    }
    pub(super) fn render(&mut self, size: RenderSize) -> anyhow::Result<Option<BgraImage>> {
        self.resources
            .dependent_mut()
            .map_or(Ok(None), |presenter| presenter.render(size))
    }
    pub(super) fn discard_pending_frames(&mut self) {
        if let Some(presenter) = self.resources.dependent_mut() {
            presenter.discard_pending();
        }
    }
    pub(super) fn presenter_snapshot(&self) -> Option<VideoPresenterSnapshot> {
        self.resources
            .dependent()
            .map(|presenter| presenter.snapshot())
    }

    #[cfg(test)]
    pub(super) fn empty() -> Self {
        Self::from_resources(None, None)
    }
}

#[cfg(test)]
pub(super) mod test_support;
#[cfg(test)]
mod tests;
