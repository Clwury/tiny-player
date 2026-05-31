use super::super::{
    backend::{
        BackendControl, BackendEvent, BackendLoadRequest, FfmpegBackend, PlaybackCacheConfig,
        PlaybackCacheState, Result,
    },
    render_host::VideoOutputQueue,
    tracks::PlaybackTrack,
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

    fn video_output_queue(&self) -> VideoOutputQueue {
        match self {
            Self::Ffmpeg(backend) => backend.video_output_queue(),
        }
    }
}

pub(super) struct ShutdownOrder<Owner, Dependent> {
    owner: Option<Owner>,
    dependent: Option<Dependent>,
}

impl<Owner, Dependent> ShutdownOrder<Owner, Dependent> {
    pub(super) fn new(owner: Option<Owner>, dependent: Option<Dependent>) -> Self {
        Self { owner, dependent }
    }

    pub(super) fn owner(&self) -> Option<&Owner> {
        self.owner.as_ref()
    }

    pub(super) fn owner_mut(&mut self) -> Option<&mut Owner> {
        self.owner.as_mut()
    }

    pub(super) fn dependent(&self) -> Option<&Dependent> {
        self.dependent.as_ref()
    }

    pub(super) fn dependent_mut(&mut self) -> Option<&mut Dependent> {
        self.dependent.as_mut()
    }
}

impl<Owner, Dependent> Drop for ShutdownOrder<Owner, Dependent> {
    fn drop(&mut self) {
        drop(self.dependent.take());
        drop(self.owner.take());
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use super::*;

    struct DropRecorder {
        name: &'static str,
        drops: Rc<RefCell<Vec<&'static str>>>,
    }

    impl Drop for DropRecorder {
        fn drop(&mut self) {
            self.drops.borrow_mut().push(self.name);
        }
    }

    #[test]
    fn shutdown_order_drops_dependent_before_owner() {
        let drops = Rc::new(RefCell::new(Vec::new()));
        let recorded_drops = Rc::clone(&drops);

        let presenter = DropRecorder {
            name: "presenter",
            drops: Rc::clone(&drops),
        };
        let backend = DropRecorder {
            name: "backend",
            drops,
        };

        drop(ShutdownOrder::new(Some(backend), Some(presenter)));

        assert_eq!(&*recorded_drops.borrow(), &["presenter", "backend"]);
    }
}
