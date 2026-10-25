//! A guest event queue on GPUI's display; never read or block its socket here.
use std::{cell::RefCell, collections::HashMap, rc::Rc, time::Duration};

use anyhow::{Context as _, Result, anyhow, ensure};
use gpui::{ActivityGuard, App, Global, Platform, Subscription, Task, Window, WindowId};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    backend::{Backend, ObjectId},
    protocol::{wl_callback, wl_registry, wl_surface::WlSurface},
};
use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1, zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
};

type SharedConnection = Rc<RefCell<Option<WindowConnection>>>;

struct Windows {
    connections: HashMap<WindowId, SharedConnection>,
    _subscriptions: [Subscription; 2],
}

impl Global for Windows {}

struct WindowConnection {
    protocol: Protocol,
    // The guest Backend borrows the native display. In the pinned GPUI,
    // WaylandClient's Cursor retains the owning Connection after its event
    // loop exits. Retain that platform through deferred task/quit cleanup.
    // Drop protocol before this owner (Rust drops fields in declaration order).
    _platform: Rc<dyn Platform>,
}

pub(super) fn request(window: &Window, cx: &mut App) -> Option<Task<Result<ActivityGuard>>> {
    let RawWindowHandle::Wayland(surface) = HasWindowHandle::window_handle(window).ok()?.as_raw()
    else {
        return None;
    };
    let RawDisplayHandle::Wayland(display) = window.display_handle().ok()?.as_raw() else {
        return None;
    };
    let platform = cx.try_global::<crate::TinyPlatform>()?.0.clone();
    if !cx.has_global::<Windows>() {
        let closed = cx.on_window_closed(|cx, id| {
            if let Some(connection) = cx.global_mut::<Windows>().connections.remove(&id) {
                connection.borrow_mut().take();
            }
        });
        let quit = cx.on_app_quit(|cx| {
            for (_, connection) in cx.global_mut::<Windows>().connections.drain() {
                connection.borrow_mut().take();
            }
            async {}
        });
        cx.set_global(Windows {
            connections: HashMap::new(),
            _subscriptions: [closed, quit],
        });
    }
    let window_id = window.window_handle().window_id();
    let connections = &mut cx.global_mut::<Windows>().connections;
    let connection = if let Some(connection) = connections.get(&window_id) {
        connection.clone()
    } else {
        // SAFETY: Both pointers come from this live GPUI window on its UI
        // thread. The platform retained above owns the display for the entire
        // guest Backend lifetime. from_ptr borrows the surface without taking
        // over its events or destroying it; wayland-rs tracks its liveness.
        let protocol = unsafe {
            let backend = Backend::from_foreign_display(display.display.as_ptr().cast());
            let connection = Connection::from_backend(backend);
            ObjectId::from_ptr(WlSurface::interface(), surface.surface.as_ptr().cast())
                .and_then(|id| WlSurface::from_id(&connection, id))
                .map_err(anyhow::Error::from)
                .and_then(|surface| Protocol::new(connection, surface))
        };
        let protocol = match protocol {
            Ok(protocol) => protocol,
            Err(error) => return Some(Task::ready(Err(error))),
        };
        let connection = Rc::new(RefCell::new(Some(WindowConnection {
            protocol,
            _platform: platform,
        })));
        connections.insert(window_id, connection.clone());
        connection
    };
    let executor = cx.background_executor().clone();
    Some(cx.spawn(async move |cx| {
        let acquisition = async {
            loop {
                if let Some(id) = connection
                    .borrow_mut()
                    .as_mut()
                    .ok_or_else(|| anyhow!("Playback window closed during Wayland inhibition"))?
                    .protocol
                    .acquire()?
                {
                    return Ok(id);
                }
                // GPUI reads the display socket and fills our private queue.
                // Dispatch only pending events, without a blocking roundtrip.
                executor.timer(Duration::from_millis(10)).await;
            }
        };
        let id = super::with_timeout(&executor, "Wayland idle inhibition", acquisition).await?;
        let (release, released) = async_channel::bounded::<()>(1);
        cx.spawn(async move |_| {
            let _ = released.recv().await;
            if let Some(connection) = connection.borrow_mut().as_mut() {
                connection.protocol.release(&id);
            }
        })
        .detach();
        Ok(ActivityGuard::new(move || {
            release.close();
        }))
    }))
}

struct Protocol {
    connection: Connection,
    queue: EventQueue<Registry>,
    state: Registry,
    _registry: wl_registry::WlRegistry,
    surface: WlSurface,
    inhibitors: HashMap<ObjectId, ZwpIdleInhibitorV1>,
}

impl Protocol {
    fn new(connection: Connection, surface: WlSurface) -> Result<Self> {
        let queue = connection.new_event_queue();
        let registry = connection.display().get_registry(&queue.handle(), ());
        connection.display().sync(&queue.handle(), ());
        let protocol = Self {
            connection,
            queue,
            state: Registry::default(),
            _registry: registry,
            surface,
            inhibitors: HashMap::new(),
        };
        protocol.connection.flush()?;
        Ok(protocol)
    }

    fn acquire(&mut self) -> Result<Option<ObjectId>> {
        self.queue.dispatch_pending(&mut self.state)?;
        self.connection.flush()?;
        if !self.state.ready {
            return Ok(None);
        }
        let manager = self
            .state
            .manager
            .as_ref()
            .context("The compositor does not support Wayland idle inhibition")?;
        ensure!(
            self.surface.is_alive(),
            "The playback Wayland surface closed"
        );
        let inhibitor = manager.create_inhibitor(&self.surface, &self.queue.handle(), ());
        ensure!(
            inhibitor.is_alive(),
            "Could not create the Wayland idle inhibitor"
        );
        let id = inhibitor.id();
        self.inhibitors.insert(id.clone(), inhibitor);
        if let Err(error) = self.connection.flush() {
            self.release(&id);
            return Err(error.into());
        }
        Ok(Some(id))
    }

    fn release(&mut self, id: &ObjectId) {
        if let Some(inhibitor) = self.inhibitors.remove(id) {
            inhibitor.destroy();
            if let Err(error) = self.connection.flush() {
                tracing::debug!(%error, "Could not flush Wayland idle inhibitor release");
            }
        }
    }
}

impl Drop for Protocol {
    fn drop(&mut self) {
        for (_, inhibitor) in self.inhibitors.drain() {
            inhibitor.destroy();
        }
        if let Some(manager) = self.state.manager.take() {
            manager.destroy();
        }
        let _ = self.connection.flush();
    }
}

#[derive(Default)]
struct Registry {
    manager: Option<ZwpIdleInhibitManagerV1>,
    ready: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Registry {
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
            && interface == ZwpIdleInhibitManagerV1::interface().name
            && state.manager.is_none()
        {
            state.manager = Some(registry.bind(name, 1, handle, ()));
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for Registry {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.ready = true;
    }
}

wayland_client::delegate_noop!(Registry: ignore ZwpIdleInhibitManagerV1);
wayland_client::delegate_noop!(Registry: ignore ZwpIdleInhibitorV1);

#[cfg(test)]
mod tests;
