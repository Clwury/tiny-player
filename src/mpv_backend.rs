use libmpv2::{
    Error as MpvError, Format, Mpv,
    events::{Event, PropertyData},
};
use std::{
    ffi::CString,
    path::{Path, PathBuf},
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
    Mpv(MpvError),
    NonUtf8Path(PathBuf),
}

pub type Result<T> = std::result::Result<T, BackendError>;

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mpv(error) => error.fmt(f),
            Self::NonUtf8Path(path) => {
                write!(f, "path contains non-UTF-8 bytes: {:?}", path)
            }
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

    pub fn load_file(&mut self, path: &Path) -> Result<()> {
        let path = path
            .to_str()
            .ok_or_else(|| BackendError::NonUtf8Path(path.to_path_buf()))?;
        self.mpv
            .set_property("pause", true)
            .map_err(BackendError::from)?;
        self.mpv.command("stop", &[]).map_err(BackendError::from)?;
        self.mpv
            .command("loadfile", &[path, "replace"])
            .map_err(BackendError::from)?;
        self.mpv
            .set_property("pause", false)
            .map_err(BackendError::from)?;
        Ok(())
    }

    pub fn pause(&mut self) -> Result<()> {
        self.mpv
            .set_property("pause", true)
            .map_err(BackendError::from)
    }

    pub fn toggle_playback(&mut self) -> Result<bool> {
        let paused = self
            .mpv
            .get_property::<bool>("pause")
            .map_err(BackendError::from)?;
        let next_paused = !paused;
        self.mpv
            .set_property("pause", next_paused)
            .map_err(BackendError::from)?;
        Ok(!next_paused)
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
    use crate::media::discover_playlist;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;
    use std::thread;
    use std::time::{Duration, Instant};

    fn sample_video_path() -> std::path::PathBuf {
        discover_playlist(&crate::sample_dir())
            .unwrap()
            .into_iter()
            .find(|entry| entry.path.extension().and_then(|ext| ext.to_str()) == Some("mp4"))
            .unwrap()
            .path
    }

    fn wait_for_events_until(
        label: &str,
        backend: &mut MpvBackend,
        predicate: impl Fn(&[BackendEvent]) -> bool,
    ) -> Vec<BackendEvent> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen_events = Vec::new();

        while Instant::now() < deadline {
            let polled = backend.poll_events();
            if !polled.is_empty() {
                seen_events.extend(polled);
                if predicate(&seen_events) {
                    return seen_events;
                }
            }

            thread::sleep(Duration::from_millis(50));
        }

        panic!("timed out waiting for {label}; saw: {seen_events:?}");
    }

    #[test]
    fn new_configures_embedded_mpv_defaults() {
        let mut backend = MpvBackend::new().unwrap();

        let vo: libmpv2::MpvStr = backend.mpv_mut().get_property("vo").unwrap();
        let keep_open: libmpv2::MpvStr = backend.mpv_mut().get_property("keep-open").unwrap();
        let paused: bool = backend.mpv_mut().get_property("pause").unwrap();

        assert_eq!(&*vo, "libmpv");
        assert_eq!(&*keep_open, "yes");
        assert!(paused);
        let events = wait_for_events_until("initial pause", &mut backend, |events| {
            events
                .iter()
                .any(|event| matches!(event, BackendEvent::Pause(true)))
        });
        assert!(
            events
                .iter()
                .any(|event| matches!(event, BackendEvent::Pause(true)))
        );
    }

    #[test]
    fn load_file_runs_until_first_frame_can_be_rendered_then_can_pause() {
        let mut backend = MpvBackend::new().unwrap();
        let path = sample_video_path();
        let expected_title = path.file_name().unwrap().to_string_lossy().into_owned();

        backend.poll_events();
        backend.load_file(&path).unwrap();

        assert!(!backend.mpv_mut().get_property::<bool>("pause").unwrap());
        let events = wait_for_events_until(
            "playback restart and title after load",
            &mut backend,
            |events| {
                events
                .iter()
                .any(|event| matches!(event, BackendEvent::PlaybackRestart))
                && events.iter().any(|event| {
                    matches!(event, BackendEvent::FileTitle(title) if title == &expected_title)
                })
            },
        );

        assert!(
            events
                .iter()
                .any(|event| matches!(event, BackendEvent::PlaybackRestart))
        );
        assert!(events.iter().any(|event| {
            matches!(event, BackendEvent::FileTitle(title) if title == &expected_title)
        }));

        backend.pause().unwrap();
        assert!(backend.mpv_mut().get_property::<bool>("pause").unwrap());
    }

    #[test]
    fn poll_events_reports_load_failures() {
        let mut backend = MpvBackend::new().unwrap();
        let missing_path = crate::sample_dir().join("missing-file.mp4");

        backend.poll_events();
        backend.load_file(&missing_path).unwrap();

        let events = wait_for_events_until("load failure", &mut backend, |events| {
            events
                .iter()
                .any(|event| matches!(event, BackendEvent::LoadFailed(_)))
        });

        assert!(events.iter().any(|event| {
            matches!(event, BackendEvent::LoadFailed(message) if !message.is_empty())
        }));
    }

    #[test]
    fn poll_events_reports_duration_changes() {
        let mut backend = MpvBackend::new().unwrap();
        let path = sample_video_path();

        backend.poll_events();
        backend.load_file(&path).unwrap();

        let events = wait_for_events_until("duration change", &mut backend, |events| {
            events.iter().any(
                |event| matches!(event, BackendEvent::DurationChanged(duration) if *duration > 0.0),
            )
        });

        assert!(events.iter().any(|event| {
            matches!(event, BackendEvent::DurationChanged(duration) if *duration > 0.0)
        }));
    }

    #[test]
    fn poll_events_reports_position_changes() {
        let mut backend = MpvBackend::new().unwrap();
        let path = sample_video_path();

        backend.poll_events();
        backend.load_file(&path).unwrap();

        let events = wait_for_events_until("position change", &mut backend, |events| {
            events.iter().any(
                |event| matches!(event, BackendEvent::PositionChanged(position) if *position > 0.0),
            )
        });

        assert!(events.iter().any(|event| {
            matches!(event, BackendEvent::PositionChanged(position) if *position > 0.0)
        }));
    }

    #[cfg(unix)]
    #[test]
    fn load_file_rejects_non_utf8_paths() {
        let mut backend = MpvBackend::new().unwrap();
        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![0x66, 0x80]));

        let error = backend
            .load_file(&path)
            .expect_err("non-UTF-8 path should be rejected before mpv");

        assert!(error.to_string().contains("UTF-8"));
    }
}
