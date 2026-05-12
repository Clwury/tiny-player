use super::*;

pub(super) enum PlaybackBackend {
    Ffmpeg(FfmpegBackend),
}

impl PlaybackBackend {
    pub(super) fn poll_events(&mut self) -> Vec<BackendEvent> {
        match self {
            Self::Ffmpeg(backend) => backend.poll_events(),
        }
    }

    pub(super) fn seek_to(&mut self, position_seconds: f64) -> super::super::backend::Result<()> {
        match self {
            Self::Ffmpeg(backend) => backend.seek_to(position_seconds),
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
