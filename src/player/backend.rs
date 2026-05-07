use std::ffi::CString;

use libmpv2::{
    Error as MpvError, Format, Mpv,
    events::{Event, PropertyData},
};

#[derive(Debug)]
pub enum BackendEvent {
    Pause(bool),
    PlaybackRestart,
    FileTitle(String),
    PositionChanged(f64),
    DurationChanged(f64),
    LoadFailed(String),
    Fatal(String),
}

#[derive(Debug)]
pub enum BackendError {
    EmptyUrl,
    Mpv(MpvError),
}

pub type Result<T> = std::result::Result<T, BackendError>;

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyUrl => write!(f, "播放地址为空"),
            Self::Mpv(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for BackendError {}

impl From<MpvError> for BackendError {
    fn from(error: MpvError) -> Self {
        Self::Mpv(error)
    }
}

pub struct MpvBackend {
    mpv: Mpv,
}

impl MpvBackend {
    pub fn new() -> Result<Self> {
        let mpv = Mpv::with_initializer(|init| {
            init.set_property("vo", "libmpv")?;
            init.set_property("keep-open", "yes")?;
            init.set_property("pause", true)?;
            Ok(())
        })
        .map_err(BackendError::from)?;

        request_log_messages(&mpv, "fatal")?;
        mpv.disable_deprecated_events()
            .map_err(BackendError::from)?;
        mpv.observe_property("pause", Format::Flag, 0)
            .map_err(BackendError::from)?;
        mpv.observe_property("media-title", Format::String, 1)
            .map_err(BackendError::from)?;
        mpv.observe_property("time-pos", Format::Double, 2)
            .map_err(BackendError::from)?;
        mpv.observe_property("duration", Format::Double, 3)
            .map_err(BackendError::from)?;

        Ok(Self { mpv })
    }

    pub fn mpv_mut(&mut self) -> &mut Mpv {
        &mut self.mpv
    }

    pub fn load_url(&mut self, url: &str) -> Result<()> {
        let url = url.trim();
        if url.is_empty() {
            return Err(BackendError::EmptyUrl);
        }

        self.mpv
            .set_property("pause", true)
            .map_err(BackendError::from)?;
        self.mpv.command("stop", &[]).map_err(BackendError::from)?;
        self.mpv
            .command("loadfile", &[url, "replace"])
            .map_err(BackendError::from)?;
        self.mpv
            .set_property("pause", false)
            .map_err(BackendError::from)?;
        Ok(())
    }

    pub fn poll_events(&mut self) -> Vec<BackendEvent> {
        let mut events = Vec::new();

        while let Some(event) = self.mpv.wait_event(0.0) {
            match event {
                Ok(Event::PropertyChange {
                    name: "pause",
                    change: PropertyData::Flag(paused),
                    ..
                }) => events.push(BackendEvent::Pause(paused)),
                Ok(Event::PlaybackRestart) => events.push(BackendEvent::PlaybackRestart),
                Ok(Event::PropertyChange {
                    name: "media-title",
                    change: PropertyData::Str(title) | PropertyData::OsdStr(title),
                    ..
                }) => events.push(BackendEvent::FileTitle(title.to_owned())),
                Ok(Event::PropertyChange {
                    name: "time-pos",
                    change: PropertyData::Double(position),
                    ..
                }) => events.push(BackendEvent::PositionChanged(position)),
                Ok(Event::PropertyChange {
                    name: "duration",
                    change: PropertyData::Double(duration),
                    ..
                }) => events.push(BackendEvent::DurationChanged(duration)),
                Ok(Event::LogMessage {
                    level: "fatal",
                    text,
                    ..
                }) => events.push(BackendEvent::Fatal(text.trim().to_owned())),
                Err(error) if is_load_failure(&error) => {
                    events.push(BackendEvent::LoadFailed(error.to_string()))
                }
                Err(error) => events.push(BackendEvent::Fatal(error.to_string())),
                _ => {}
            }
        }

        events
    }
}

fn is_load_failure(error: &MpvError) -> bool {
    matches!(
        error,
        MpvError::Loadfile { .. } | MpvError::Raw(libmpv2::mpv_error::LoadingFailed)
    )
}

fn request_log_messages(mpv: &Mpv, min_level: &str) -> std::result::Result<(), MpvError> {
    let min_level = CString::new(min_level).map_err(MpvError::from)?;
    let error =
        unsafe { libmpv2_sys::mpv_request_log_messages(mpv.ctx.as_ptr(), min_level.as_ptr()) };

    if error == 0 {
        Ok(())
    } else {
        Err(MpvError::Raw(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_url_error_has_user_facing_message() {
        assert_eq!(BackendError::EmptyUrl.to_string(), "播放地址为空");
    }
}
