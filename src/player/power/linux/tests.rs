use std::sync::{Arc, Mutex};

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mechanism {
    Window,
    IdlePortal,
    SuspendPortal,
    ScreenSaver,
    Logind,
}

#[derive(Default)]
struct Backend {
    supported: Vec<Mechanism>,
    calls: Mutex<Vec<Mechanism>>,
    active: Arc<Mutex<Vec<Mechanism>>>,
    pending_suspend: bool,
}

impl Backend {
    fn guard(&self, mechanism: Mechanism) -> ActivityGuard {
        self.active.lock().unwrap().push(mechanism);
        let active = self.active.clone();
        ActivityGuard::new(move || {
            let mut active = active.lock().unwrap();
            let index = active.iter().position(|item| *item == mechanism).unwrap();
            active.remove(index);
        })
    }

    fn attempt(&self, mechanism: Mechanism) -> Result<ActivityGuard> {
        self.calls.lock().unwrap().push(mechanism);
        if self.supported.contains(&mechanism) {
            Ok(self.guard(mechanism))
        } else {
            Err(anyhow!("{mechanism:?} is unsupported"))
        }
    }

    fn active(&self) -> Vec<Mechanism> {
        self.active.lock().unwrap().clone()
    }
}

impl InhibitionBackend for Backend {
    async fn portal(&self, flags: InhibitFlags) -> Result<ActivityGuard> {
        match flags {
            InhibitFlags::Idle => self.attempt(Mechanism::IdlePortal),
            InhibitFlags::Suspend if self.pending_suspend => std::future::pending().await,
            InhibitFlags::Suspend => self.attempt(Mechanism::SuspendPortal),
            _ => panic!("Playback must request only Idle or Suspend independently"),
        }
    }

    async fn screensaver(&self) -> Result<ActivityGuard> {
        self.attempt(Mechanism::ScreenSaver)
    }

    async fn logind(&self) -> Result<ActivityGuard> {
        self.attempt(Mechanism::Logind)
    }
}

#[test]
fn gtk_idle_only_portal_keeps_idle_when_suspend_and_logind_are_rejected() {
    let backend = Backend {
        supported: vec![Mechanism::IdlePortal],
        ..Default::default()
    };
    let guard = futures_lite::future::block_on(acquire(&backend, async { Ok(None) })).unwrap();
    assert_eq!(backend.active(), [Mechanism::IdlePortal]);
    assert_eq!(
        *backend.calls.lock().unwrap(),
        [
            Mechanism::IdlePortal,
            Mechanism::SuspendPortal,
            Mechanism::Logind
        ]
    );
    drop(guard);
    assert!(backend.active().is_empty());
}

#[test]
fn independent_portal_requests_do_not_use_fallbacks_when_both_succeed() {
    let backend = Backend {
        supported: vec![Mechanism::IdlePortal, Mechanism::SuspendPortal],
        ..Default::default()
    };
    let guard = futures_lite::future::block_on(acquire(&backend, async { Ok(None) })).unwrap();
    assert_eq!(backend.active(), backend.supported);
    assert_eq!(*backend.calls.lock().unwrap(), backend.supported);
    drop(guard);
    assert!(backend.active().is_empty());
}

#[test]
fn unavailable_portal_falls_back_to_session_screensaver_and_logind_sleep() {
    let backend = Backend {
        supported: vec![Mechanism::ScreenSaver, Mechanism::Logind],
        ..Default::default()
    };
    let guard = futures_lite::future::block_on(acquire(&backend, async { Ok(None) })).unwrap();
    assert_eq!(backend.active(), backend.supported);
    assert_eq!(
        *backend.calls.lock().unwrap(),
        [
            Mechanism::IdlePortal,
            Mechanism::ScreenSaver,
            Mechanism::SuspendPortal,
            Mechanism::Logind,
        ]
    );
    drop(guard);
    assert!(backend.active().is_empty());
}

#[test]
fn wayland_only_compositor_keeps_window_inhibition_without_any_bus_service() {
    let backend = Backend::default();
    let window = backend.guard(Mechanism::Window);
    let guard =
        futures_lite::future::block_on(acquire(&backend, async { Ok(Some(window)) })).unwrap();
    assert_eq!(backend.active(), [Mechanism::Window]);
    drop(guard);
    assert!(backend.active().is_empty());
}

#[test]
fn wayland_failure_does_not_discard_successful_session_and_sleep_guards() {
    let backend = Backend {
        supported: vec![Mechanism::IdlePortal, Mechanism::Logind],
        ..Default::default()
    };
    let guard = futures_lite::future::block_on(acquire(&backend, async {
        Err(anyhow!("No Wayland idle-inhibit global"))
    }))
    .unwrap();
    assert_eq!(backend.active(), backend.supported);
    drop(guard);
    assert!(backend.active().is_empty());
}

#[test]
fn cancelling_pending_acquisition_releases_already_accepted_idle_requests() {
    let backend = Backend {
        supported: vec![Mechanism::IdlePortal],
        pending_suspend: true,
        ..Default::default()
    };
    let window = backend.guard(Mechanism::Window);
    let mut acquisition = Box::pin(acquire(&backend, async { Ok(Some(window)) }));
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(acquisition.as_mut().poll(&mut context).is_pending());
    assert_eq!(backend.active(), [Mechanism::Window, Mechanism::IdlePortal]);
    drop(acquisition);
    assert!(backend.active().is_empty());
}

#[test]
fn no_supported_mechanism_reports_failure_without_leaking_guards() {
    let backend = Backend::default();
    assert!(futures_lite::future::block_on(acquire(&backend, async { Ok(None) })).is_err());
    assert!(backend.active().is_empty());
}

#[gpui::test]
fn timed_out_request_is_cancelled_and_other_accepted_guards_remain_active(
    cx: &mut gpui::TestAppContext,
) {
    let executor = cx.background_executor.clone();
    let backend = Backend::default();
    let idle = backend.guard(Mechanism::IdlePortal);
    let guard = backend.guard(Mechanism::SuspendPortal);
    let request = executor.clone().spawn(async move {
        with_timeout(&executor, "test inhibition", async move {
            let _guard = guard;
            std::future::pending::<Result<()>>().await
        })
        .await
    });
    cx.run_until_parked();
    cx.executor().advance_clock(REQUEST_TIMEOUT);
    cx.run_until_parked();
    assert!(futures_lite::future::block_on(request).is_err());
    assert_eq!(backend.active(), [Mechanism::IdlePortal]);
    drop(idle);
    assert!(backend.active().is_empty());
}
