use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use ashpd::desktop::inhibit::{InhibitFlags, InhibitOptions, InhibitProxy};
use gpui::{ActivityGuard, BackgroundExecutor, Task};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn request(executor: BackgroundExecutor) -> Task<Result<ActivityGuard>> {
    executor.clone().spawn(async move {
        futures_lite::future::or(acquire(executor.clone()), async move {
            executor.timer(REQUEST_TIMEOUT).await;
            Err(anyhow!(
                "Playback idle inhibition timed out after 10 seconds"
            ))
        })
        .await
    })
}

async fn acquire(executor: BackgroundExecutor) -> Result<ActivityGuard> {
    // Use a private connection: cancellation while awaiting the portal response
    // must also remove any inhibition the desktop has already accepted.
    let connection = zbus::connection::Builder::session()?
        .method_timeout(REQUEST_TIMEOUT)
        .build()
        .await
        .context("Could not connect to the desktop session bus")?;
    let proxy = InhibitProxy::with_connection(connection)
        .await
        .context("The desktop idle inhibition portal is unavailable")?;
    let request = proxy
        .inhibit(
            None,
            InhibitFlags::Idle | InhibitFlags::Suspend,
            InhibitOptions::default().set_reason(super::PLAYBACK_POWER_REASON),
        )
        .await
        .context("Could not request playback idle inhibition")?;
    let response = request.response();
    let guard = ActivityGuard::new(move || {
        executor
            .spawn(async move {
                if let Err(error) = request.close().await {
                    tracing::warn!(%error, "Could not release playback idle inhibition");
                }
            })
            .detach();
    });
    response.context("The desktop rejected playback idle inhibition")?;
    Ok(guard)
}
