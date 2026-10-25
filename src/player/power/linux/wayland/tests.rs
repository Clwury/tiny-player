//! A small headless compositor exercises the real Wayland protocol and guest queue.
use std::{
    io::{Read as _, Write as _},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

use wayland_client::protocol::wl_compositor::WlCompositor;

use super::*;

#[derive(Default)]
struct Observed {
    registries: usize,
    created: usize,
    destroyed: usize,
    active: HashMap<u32, u32>,
}

struct Server {
    socket: UnixStream,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
        self.thread.take().unwrap().join().unwrap();
    }
}

struct Fixture {
    protocol: Option<Protocol>,
    surface: WlSurface,
    host: Connection,
    observed: Arc<Mutex<Observed>>,
    _server: Server,
}

impl Fixture {
    fn new(has_manager: bool) -> Self {
        let (client, mut socket) = UnixStream::pair().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let control = socket.try_clone().unwrap();
        let observed = Arc::new(Mutex::new(Observed::default()));
        let state = observed.clone();
        let thread = std::thread::spawn(move || {
            let mut objects = HashMap::from([(1_u32, "wl_display".to_owned())]);
            loop {
                let mut header = [0; 8];
                if socket.read_exact(&mut header).is_err() {
                    break;
                }
                let object = word(&header, 0);
                let size_opcode = word(&header, 1);
                let opcode = size_opcode & 0xffff;
                let mut body = vec![0; (size_opcode >> 16) as usize - 8];
                socket.read_exact(&mut body).unwrap();
                match (objects.get(&object).unwrap().as_str(), opcode) {
                    ("wl_display", 0) => {
                        let callback = word(&body, 0);
                        event(&mut socket, callback, 0, &0_u32.to_ne_bytes());
                        event(&mut socket, 1, 1, &callback.to_ne_bytes());
                    }
                    ("wl_display", 1) => {
                        let registry = word(&body, 0);
                        objects.insert(registry, "wl_registry".into());
                        state.lock().unwrap().registries += 1;
                        global(&mut socket, registry, 1, "wl_compositor");
                        if has_manager {
                            global(&mut socket, registry, 2, "zwp_idle_inhibit_manager_v1");
                        }
                    }
                    ("wl_registry", 0) => {
                        let length = word(&body, 1) as usize;
                        let interface = std::str::from_utf8(&body[8..8 + length - 1]).unwrap();
                        let id = word(&body, (8 + length.next_multiple_of(4) + 4) / 4);
                        objects.insert(id, interface.into());
                    }
                    ("wl_compositor", 0) => {
                        objects.insert(word(&body, 0), "wl_surface".into());
                    }
                    ("zwp_idle_inhibit_manager_v1", 1) => {
                        let id = word(&body, 0);
                        let surface = word(&body, 1);
                        assert_eq!(objects.get(&surface).unwrap(), "wl_surface");
                        objects.insert(id, "zwp_idle_inhibitor_v1".into());
                        let mut state = state.lock().unwrap();
                        state.created += 1;
                        state.active.insert(id, surface);
                    }
                    ("zwp_idle_inhibitor_v1", 0) => {
                        let mut state = state.lock().unwrap();
                        assert!(state.active.remove(&object).is_some());
                        state.destroyed += 1;
                        objects.remove(&object);
                        event(&mut socket, 1, 1, &object.to_ne_bytes());
                    }
                    ("zwp_idle_inhibit_manager_v1" | "wl_surface", 0) => {
                        objects.remove(&object);
                        event(&mut socket, 1, 1, &object.to_ne_bytes());
                    }
                    (interface, opcode) => panic!("Unexpected {interface}.{opcode}"),
                }
            }
        });
        let host = Connection::from_socket(client).unwrap();
        let mut queue = host.new_event_queue();
        host.display().get_registry(&queue.handle(), ());
        let mut bootstrap = Bootstrap::default();
        queue.roundtrip(&mut bootstrap).unwrap();
        let surface = bootstrap
            .compositor
            .unwrap()
            .create_surface(&queue.handle(), ());
        // SAFETY: The fixture retains host until after protocol is dropped,
        // exactly as the production wrapper retains GPUI's display owner.
        let guest = unsafe {
            Connection::from_backend(Backend::from_foreign_display(host.backend().display_ptr()))
        };
        let guest_surface = WlSurface::from_id(&guest, surface.id()).unwrap();
        let protocol = Protocol::new(guest, guest_surface).unwrap();
        Self {
            protocol: Some(protocol),
            surface,
            host,
            observed,
            _server: Server {
                socket: control,
                thread: Some(thread),
            },
        }
    }

    fn acquire(&mut self) -> Result<ObjectId> {
        self.host.roundtrip().unwrap();
        self.protocol
            .as_mut()
            .unwrap()
            .acquire()?
            .context("Registry did not finish")
    }
}

fn word(bytes: &[u8], index: usize) -> u32 {
    u32::from_ne_bytes(bytes[index * 4..index * 4 + 4].try_into().unwrap())
}

fn event(socket: &mut UnixStream, object: u32, opcode: u32, body: &[u8]) {
    let size_opcode = (((body.len() + 8) as u32) << 16) | opcode;
    let message = [
        object.to_ne_bytes().as_slice(),
        &size_opcode.to_ne_bytes(),
        body,
    ]
    .concat();
    // The client may close its connection while destruction events are sent.
    let _ = socket.write_all(&message);
}

fn global(socket: &mut UnixStream, registry: u32, name: u32, interface: &str) {
    let mut body = name.to_ne_bytes().to_vec();
    body.extend_from_slice(&((interface.len() + 1) as u32).to_ne_bytes());
    body.extend_from_slice(interface.as_bytes());
    body.push(0);
    body.resize(body.len().next_multiple_of(4), 0);
    body.extend_from_slice(&1_u32.to_ne_bytes());
    event(socket, registry, 0, &body);
}

#[derive(Default)]
struct Bootstrap {
    compositor: Option<WlCompositor>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Bootstrap {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
            && interface == "wl_compositor"
        {
            state.compositor = Some(registry.bind(name, 1, handle, ()));
        }
    }
}

wayland_client::delegate_noop!(Bootstrap: ignore WlCompositor);
wayland_client::delegate_noop!(Bootstrap: ignore WlSurface);

#[test]
fn guest_queue_inhibits_the_gpui_surface_and_reuses_registry_after_pause_resume() {
    let mut fixture = Fixture::new(true);
    for _ in 0..3 {
        let id = fixture.acquire().unwrap();
        fixture.host.roundtrip().unwrap();
        assert_eq!(
            fixture
                .observed
                .lock()
                .unwrap()
                .active
                .values()
                .copied()
                .collect::<Vec<_>>(),
            [fixture.surface.id().protocol_id()]
        );
        fixture.protocol.as_mut().unwrap().release(&id);
        fixture.host.roundtrip().unwrap();
        assert!(fixture.observed.lock().unwrap().active.is_empty());
    }
    let state = fixture.observed.lock().unwrap();
    assert_eq!(state.registries, 2); // One host registry and one cached guest registry.
    assert_eq!((state.created, state.destroyed), (3, 3));
}

#[test]
fn closing_the_window_context_releases_every_outstanding_inhibitor() {
    let mut fixture = Fixture::new(true);
    fixture.acquire().unwrap();
    fixture.acquire().unwrap();
    fixture.host.roundtrip().unwrap();
    assert_eq!(fixture.observed.lock().unwrap().active.len(), 2);
    fixture.protocol.take();
    fixture.host.roundtrip().unwrap();
    assert!(fixture.observed.lock().unwrap().active.is_empty());
}

#[test]
fn compositor_without_idle_inhibit_returns_an_error_for_session_fallback() {
    let mut fixture = Fixture::new(false);
    assert!(fixture.acquire().is_err());
    assert!(fixture.observed.lock().unwrap().active.is_empty());
}

#[test]
fn destroyed_gpui_surface_is_not_used_for_a_late_acquisition() {
    let mut fixture = Fixture::new(true);
    fixture.surface.destroy();
    assert!(fixture.acquire().is_err());
    assert!(fixture.observed.lock().unwrap().active.is_empty());
}
