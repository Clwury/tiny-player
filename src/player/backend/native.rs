use tiny_playback::{
    BackendControl, BackendEvent, BackendLoadRequest, FfmpegBackend, PlaybackCacheConfig,
    PlaybackCacheState, PlaybackTrack, Result, VideoOutput,
};

pub(super) enum PlaybackBackend {
    Ffmpeg(FfmpegBackend),
}

impl BackendControl for PlaybackBackend {
    fn load(&mut self, request: BackendLoadRequest) -> Result<()> {
        match self {
            Self::Ffmpeg(backend) => backend.load(request),
        }
    }

    fn seek(&mut self, position_seconds: f64) -> Result<()> {
        match self {
            Self::Ffmpeg(backend) => backend.seek(position_seconds),
        }
    }

    fn pause(&mut self) -> Result<()> {
        match self {
            Self::Ffmpeg(backend) => backend.pause(),
        }
    }

    fn resume(&mut self) -> Result<()> {
        match self {
            Self::Ffmpeg(backend) => backend.resume(),
        }
    }

    fn stop(&mut self) -> Result<()> {
        match self {
            Self::Ffmpeg(backend) => backend.stop(),
        }
    }

    fn set_audio_track(&mut self, track_index: Option<usize>, position_seconds: f64) -> Result<()> {
        match self {
            Self::Ffmpeg(backend) => backend.set_audio_track(track_index, position_seconds),
        }
    }

    fn set_subtitle_track(
        &mut self,
        track: Option<PlaybackTrack>,
        position_seconds: f64,
    ) -> Result<()> {
        match self {
            Self::Ffmpeg(backend) => backend.set_subtitle_track(track, position_seconds),
        }
    }

    fn set_volume(&mut self, volume: f32) -> Result<()> {
        match self {
            Self::Ffmpeg(backend) => backend.set_volume(volume),
        }
    }

    fn set_playback_rate(&mut self, rate: f64) -> Result<()> {
        match self {
            Self::Ffmpeg(backend) => backend.set_playback_rate(rate),
        }
    }

    fn set_cache_config(&mut self, config: PlaybackCacheConfig) -> Result<()> {
        match self {
            Self::Ffmpeg(backend) => backend.set_cache_config(config),
        }
    }

    fn cache_state(&self) -> Option<PlaybackCacheState> {
        match self {
            Self::Ffmpeg(backend) => backend.cache_state(),
        }
    }

    fn poll_events(&mut self) -> Vec<BackendEvent> {
        match self {
            Self::Ffmpeg(backend) => backend.poll_events(),
        }
    }

    fn video_output(&self) -> VideoOutput {
        match self {
            Self::Ffmpeg(backend) => backend.video_output(),
        }
    }
}

impl super::PlaybackDriver for PlaybackBackend {
    fn command(&mut self, command: tiny_playback::BackendCommand) -> Result<()> {
        BackendControl::command(self, command)
    }
    fn poll_events(&mut self) -> Vec<BackendEvent> {
        // FFmpeg filters stale worker session IDs before returning these events.
        BackendControl::poll_events(self)
    }
    fn create_presenter(&self) -> anyhow::Result<Box<dyn super::FramePresenter>> {
        Ok(Box::new(tiny_playback::VideoPresenter::new(
            self.video_output(),
        )?))
    }
}

impl super::FramePresenter for tiny_playback::VideoPresenter {
    fn prewarm(&mut self) {
        self.prewarm_if_needed();
    }
    fn render(
        &mut self,
        size: tiny_playback::RenderSize,
    ) -> anyhow::Result<Option<tiny_playback::BgraImage>> {
        self.render_if_needed(size)
    }
    fn discard_pending(&mut self) {
        self.discard_pending_frames();
    }
    fn snapshot(&self) -> tiny_playback::VideoPresenterSnapshot {
        self.snapshot()
    }
}
