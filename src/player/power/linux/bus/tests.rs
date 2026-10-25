//! Exercise the actual D-Bus messages on an isolated bus, without a desktop.
use std::{
    collections::HashMap,
    io::{BufRead as _, BufReader, Read as _},
    os::unix::net::UnixStream,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use zbus::{message::Header, zvariant::OwnedValue};

use super::*;

fn executor() -> BackgroundExecutor {
    static EXECUTOR: OnceLock<BackgroundExecutor> = OnceLock::new();
    EXECUTOR
        .get_or_init(|| BackgroundExecutor::new(Arc::new(gpui::ThreadedDispatcher::new())))
        .clone()
}

fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < deadline, "D-Bus inhibition test timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}

struct Bus {
    daemon: Child,
    address: String,
    _directory: tempfile::TempDir,
}

impl Bus {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut daemon = Command::new("dbus-daemon")
            .args([
                "--session",
                "--nofork",
                "--nopidfile",
                "--print-address=1",
                &format!("--address=unix:path={}/bus", directory.path().display()),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("D-Bus integration tests require dbus-daemon");
        let mut address = String::new();
        BufReader::new(daemon.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        if address.is_empty() {
            let output = daemon.wait_with_output().unwrap();
            panic!(
                "Private D-Bus daemon did not start ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Self {
            daemon,
            address: address.trim().into(),
            _directory: directory,
        }
    }

    fn inhibitors(&self) -> Inhibitors {
        Inhibitors {
            executor: executor(),
            address: Some(self.address.clone()),
        }
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

#[derive(Clone, Copy)]
enum PortalMode {
    IdleOnly,
    RejectedResponse,
    Pending,
}

#[derive(Default)]
struct State {
    calls: Mutex<Vec<String>>,
    sender: Mutex<Option<String>>,
    sleep_peer: Mutex<Option<UnixStream>>,
}

impl State {
    fn record(&self, call: impl Into<String>) {
        self.calls.lock().unwrap().push(call.into());
    }

    fn called(&self, call: &str) -> bool {
        self.calls.lock().unwrap().iter().any(|item| item == call)
    }
}

struct Portal {
    mode: PortalMode,
    state: Arc<State>,
}

#[zbus::interface(name = "org.freedesktop.portal.Inhibit")]
impl Portal {
    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    async fn inhibit(
        &self,
        window: &str,
        flags: u32,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedObjectPath> {
        assert!(window.is_empty());
        assert_eq!(
            <&str>::try_from(options.get("reason").unwrap()).unwrap(),
            crate::player::power::PLAYBACK_POWER_REASON
        );
        let sender = header.sender().unwrap();
        *self.state.sender.lock().unwrap() = Some(sender.to_string());
        self.state.record(format!("portal:{flags}"));
        if matches!(self.mode, PortalMode::IdleOnly) && flags != 8 {
            return Err(zbus::fdo::Error::Failed(
                "Inhibiting other than idle not supported".into(),
            ));
        }
        if matches!(self.mode, PortalMode::Pending) {
            std::future::pending::<()>().await;
        }
        let token = <&str>::try_from(options.get("handle_token").unwrap()).unwrap();
        let sender_path = sender.as_str().trim_start_matches(':').replace('.', "_");
        let path = zbus::zvariant::OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/request/{sender_path}/{token}"
        ))
        .unwrap();
        connection
            .object_server()
            .at(
                path.clone(),
                Request {
                    flags,
                    state: self.state.clone(),
                },
            )
            .await?;
        let response = if matches!(self.mode, PortalMode::RejectedResponse) {
            2_u32
        } else {
            0
        };
        connection
            .emit_signal(
                Some(sender.as_str()),
                path.clone(),
                "org.freedesktop.portal.Request",
                "Response",
                &(response, HashMap::<String, OwnedValue>::new()),
            )
            .await?;
        Ok(path)
    }
}

struct Request {
    flags: u32,
    state: Arc<State>,
}

#[zbus::interface(name = "org.freedesktop.portal.Request")]
impl Request {
    fn close(&self) {
        self.state.record(format!("close:{}", self.flags));
    }
}

struct ScreenSaver(Arc<State>);

#[zbus::interface(name = "org.freedesktop.ScreenSaver")]
impl ScreenSaver {
    fn inhibit(&self, application: &str, reason: &str) -> u32 {
        assert_eq!(application, "Tiny Player");
        assert_eq!(reason, crate::player::power::PLAYBACK_POWER_REASON);
        self.0.record("screensaver");
        42
    }

    fn un_inhibit(&self, cookie: u32) {
        assert_eq!(cookie, 42);
        self.0.record("uninhibit");
    }
}

struct Logind(Arc<State>);

#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl Logind {
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::zvariant::OwnedFd {
        assert_eq!((what, mode), ("sleep", "block"));
        assert_eq!(who, "Tiny Player");
        assert_eq!(why, crate::player::power::PLAYBACK_POWER_REASON);
        let (fd, peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        *self.0.sleep_peer.lock().unwrap() = Some(peer);
        self.0.record("sleep");
        std::os::fd::OwnedFd::from(fd).into()
    }
}

fn service(bus: &Bus, mode: PortalMode, state: Arc<State>) -> zbus::Connection {
    futures_lite::future::block_on(async {
        zbus::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name("org.freedesktop.portal.Desktop")
            .unwrap()
            .name(SCREENSAVER)
            .unwrap()
            .name(LOGIND)
            .unwrap()
            .serve_at(
                "/org/freedesktop/portal/desktop",
                Portal {
                    mode,
                    state: state.clone(),
                },
            )
            .unwrap()
            .serve_at(SCREENSAVER_PATH, ScreenSaver(state.clone()))
            .unwrap()
            .serve_at(LOGIND_PATH, Logind(state))
            .unwrap()
            .build()
            .await
            .unwrap()
    })
}

#[test]
fn gtk_portal_receives_separate_flags_and_sleep_fd_closes_with_the_guard() {
    let bus = Bus::new();
    let state = Arc::new(State::default());
    let _service = service(&bus, PortalMode::IdleOnly, state.clone());
    let backend = bus.inhibitors();
    let guard = futures_lite::future::block_on(super::super::acquire(&backend, async { Ok(None) }))
        .unwrap();
    assert!(state.called("portal:8"));
    assert!(state.called("portal:4"));
    assert!(!state.called("portal:12"));
    assert!(!state.called("screensaver"));
    assert!(state.called("sleep"));
    assert!(!state.called("close:8"));
    drop(guard);
    wait_for(|| state.called("close:8"));
    wait_for(|| {
        matches!(
            state
                .sleep_peer
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .read(&mut [0]),
            Ok(0)
        )
    });
}

#[test]
fn rejected_portal_response_uses_screensaver_cookie_and_releases_it() {
    let bus = Bus::new();
    let state = Arc::new(State::default());
    let _service = service(&bus, PortalMode::RejectedResponse, state.clone());
    let guard =
        futures_lite::future::block_on(super::super::session_idle(&bus.inhibitors())).unwrap();
    assert!(state.called("screensaver"));
    assert!(!state.called("uninhibit"));
    drop(guard);
    wait_for(|| state.called("uninhibit"));
    wait_for(|| state.called("close:8"));
}

#[test]
fn cancelling_an_in_flight_portal_request_disconnects_its_private_bus_peer() {
    let bus = Bus::new();
    let state = Arc::new(State::default());
    let service = service(&bus, PortalMode::Pending, state.clone());
    let backend = bus.inhibitors();
    let task = executor().spawn(async move { backend.acquire_portal(InhibitFlags::Idle).await });
    wait_for(|| state.called("portal:8"));
    let sender = state.sender.lock().unwrap().clone().unwrap();
    drop(task);
    let proxy = futures_lite::future::block_on(zbus::fdo::DBusProxy::new(&service)).unwrap();
    wait_for(|| {
        !futures_lite::future::block_on(proxy.name_has_owner(sender.as_str().try_into().unwrap()))
            .unwrap()
    });
}
