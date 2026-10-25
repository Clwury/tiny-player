//! Independent window idle, session idle, and system sleep inhibitors.
use std::{future::Future, time::Duration};

use anyhow::{Result, anyhow};
use ashpd::desktop::inhibit::InhibitFlags;
use gpui::{ActivityGuard, App, BackgroundExecutor, Task, Window};

mod bus;
mod wayland;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn request(window: &Window, cx: &mut App) -> Task<Result<ActivityGuard>> {
    let wayland = wayland::request(window, cx);
    let bus = bus::Inhibitors::new(cx.background_executor().clone());
    cx.spawn(async move |_| {
        let window_idle = async move {
            match wayland {
                Some(request) => request.await.map(Some),
                None => Ok(None),
            }
        };
        acquire(&bus, window_idle).await
    })
}

trait InhibitionBackend {
    fn portal(&self, flags: InhibitFlags) -> impl Future<Output = Result<ActivityGuard>>;
    fn screensaver(&self) -> impl Future<Output = Result<ActivityGuard>>;
    fn logind(&self) -> impl Future<Output = Result<ActivityGuard>>;
}

async fn session_idle(backend: &impl InhibitionBackend) -> Result<ActivityGuard> {
    match backend.portal(InhibitFlags::Idle).await {
        Ok(guard) => Ok(guard),
        Err(error) => {
            tracing::debug!(%error, "Idle portal failed; trying the session ScreenSaver service");
            backend.screensaver().await
        }
    }
}

async fn system_sleep(backend: &impl InhibitionBackend) -> Result<ActivityGuard> {
    match backend.portal(InhibitFlags::Suspend).await {
        Ok(guard) => Ok(guard),
        Err(error) => {
            tracing::debug!(%error, "Suspend portal failed; trying logind sleep inhibition");
            backend.logind().await
        }
    }
}

async fn acquire(
    backend: &impl InhibitionBackend,
    window_idle: impl Future<Output = Result<Option<ActivityGuard>>>,
) -> Result<ActivityGuard> {
    // zip retains completed guards while the other independent requests finish.
    // Cancelling this future drops both completed guards and pending requests.
    let (window, (idle, sleep)) = futures_lite::future::zip(
        window_idle,
        futures_lite::future::zip(session_idle(backend), system_sleep(backend)),
    )
    .await;
    let mut guards = Vec::new();
    match window {
        Ok(Some(guard)) => guards.push(guard),
        Ok(None) => {}
        Err(error) => tracing::warn!(%error, "Could not inhibit Wayland window idle"),
    }
    for (kind, result) in [("session idle", idle), ("system sleep", sleep)] {
        match result {
            Ok(guard) => guards.push(guard),
            Err(error) => {
                tracing::warn!(%error, kind, "Could not acquire playback power inhibition")
            }
        }
    }
    if guards.is_empty() {
        return Err(anyhow!(
            "No playback power inhibition mechanism is available"
        ));
    }
    Ok(ActivityGuard::new(move || drop(guards)))
}

async fn with_timeout<T>(
    executor: &BackgroundExecutor,
    operation: &'static str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    futures_lite::future::or(future, async {
        executor.timer(REQUEST_TIMEOUT).await;
        Err(anyhow!("{operation} timed out after 10 seconds"))
    })
    .await
}

#[cfg(test)]
mod tests;
