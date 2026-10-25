//! Private D-Bus connections make cancellation release accepted requests too.
use anyhow::{Context as _, Result};
use ashpd::desktop::inhibit::{InhibitFlags, InhibitOptions, InhibitProxy};
use gpui::{ActivityGuard, BackgroundExecutor};

use super::{InhibitionBackend, REQUEST_TIMEOUT, with_timeout};

const SCREENSAVER: &str = "org.freedesktop.ScreenSaver";
const SCREENSAVER_PATH: &str = "/org/freedesktop/ScreenSaver";
const LOGIND: &str = "org.freedesktop.login1";
const LOGIND_PATH: &str = "/org/freedesktop/login1";
const LOGIND_MANAGER: &str = "org.freedesktop.login1.Manager";

pub(super) struct Inhibitors {
    executor: BackgroundExecutor,
    #[cfg(test)]
    address: Option<String>,
}

impl Inhibitors {
    pub(super) fn new(executor: BackgroundExecutor) -> Self {
        Self {
            executor,
            #[cfg(test)]
            address: None,
        }
    }

    async fn connect(&self, system: bool) -> Result<PrivateConnection> {
        #[cfg(test)]
        let address = self.address.as_deref();
        #[cfg(not(test))]
        let address: Option<&str> = None;
        let builder = match address {
            Some(address) => zbus::connection::Builder::address(address)?,
            None if system => zbus::connection::Builder::system()?,
            None => zbus::connection::Builder::session()?,
        };
        let connection = builder
            .method_timeout(REQUEST_TIMEOUT)
            .build()
            .await
            .context("Could not connect to the playback inhibition bus")?;
        Ok(PrivateConnection {
            connection,
            executor: self.executor.clone(),
        })
    }

    async fn acquire_portal(&self, flags: InhibitFlags) -> Result<ActivityGuard> {
        let connection = self.connect(false).await?;
        let proxy = InhibitProxy::with_connection(connection.connection.clone())
            .await
            .context("The desktop inhibition portal is unavailable")?;
        // Never combine Idle and Suspend: GTK's ScreenSaver backend rejects
        // the entire request when any non-Idle flag is present.
        let request = proxy
            .inhibit(
                None,
                flags.into(),
                InhibitOptions::default().set_reason(super::super::PLAYBACK_POWER_REASON),
            )
            .await
            .context("Could not request portal playback inhibition")?;
        let response = request.response();
        let executor = self.executor.clone();
        let guard = ActivityGuard::new(move || {
            executor
                .spawn(async move {
                    if let Err(error) = request.close().await {
                        tracing::warn!(%error, "Could not close the playback portal request");
                    }
                    drop(connection);
                })
                .detach();
        });
        response.context("The desktop rejected playback inhibition")?;
        Ok(guard)
    }

    async fn acquire_screensaver(&self) -> Result<ActivityGuard> {
        let connection = self.connect(false).await?;
        let proxy = zbus::Proxy::new_owned(
            connection.connection.clone(),
            SCREENSAVER,
            SCREENSAVER_PATH,
            SCREENSAVER,
        )
        .await?;
        let cookie: u32 = proxy
            .call(
                "Inhibit",
                &("Tiny Player", super::super::PLAYBACK_POWER_REASON),
            )
            .await
            .context("Could not inhibit the session ScreenSaver")?;
        let executor = self.executor.clone();
        Ok(ActivityGuard::new(move || {
            executor
                .spawn(async move {
                    if let Err(error) = proxy.call::<_, _, ()>("UnInhibit", &(cookie,)).await {
                        tracing::warn!(%error, "Could not release the playback ScreenSaver request");
                    }
                    drop(connection);
                })
                .detach();
        }))
    }

    async fn acquire_logind(&self) -> Result<ActivityGuard> {
        let connection = self.connect(true).await?;
        let proxy =
            zbus::Proxy::new(&connection.connection, LOGIND, LOGIND_PATH, LOGIND_MANAGER).await?;
        let fd: zbus::zvariant::OwnedFd = proxy
            .call(
                "Inhibit",
                &(
                    "sleep",
                    "Tiny Player",
                    super::super::PLAYBACK_POWER_REASON,
                    "block",
                ),
            )
            .await
            .context("Could not inhibit automatic sleep through logind")?;
        // logind holds inhibition for the descriptor lifetime, independently of
        // the D-Bus connection. Do not request logind idle: Wayland idle
        // notifiers need the window/ScreenSaver mechanisms instead.
        Ok(ActivityGuard::new(move || drop(fd)))
    }
}

impl InhibitionBackend for Inhibitors {
    async fn portal(&self, flags: InhibitFlags) -> Result<ActivityGuard> {
        with_timeout(
            &self.executor,
            "Portal inhibition",
            self.acquire_portal(flags),
        )
        .await
    }

    async fn screensaver(&self) -> Result<ActivityGuard> {
        with_timeout(
            &self.executor,
            "ScreenSaver inhibition",
            self.acquire_screensaver(),
        )
        .await
    }

    async fn logind(&self) -> Result<ActivityGuard> {
        with_timeout(
            &self.executor,
            "logind sleep inhibition",
            self.acquire_logind(),
        )
        .await
    }
}

struct PrivateConnection {
    connection: zbus::Connection,
    executor: BackgroundExecutor,
}

impl Drop for PrivateConnection {
    fn drop(&mut self) {
        let connection = self.connection.clone();
        self.executor
            .spawn(async move {
                if let Err(error) = connection.close().await {
                    tracing::debug!(%error, "Could not close the playback inhibition bus");
                }
            })
            .detach();
    }
}

#[cfg(test)]
mod tests;
