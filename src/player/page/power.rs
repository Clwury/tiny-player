use anyhow::Result;
use gpui::{ActivityGuard, App, Task};

use super::*;

type AcquirePower = fn(&Window, &App) -> Task<Result<ActivityGuard>>;

pub(super) struct PlaybackPower {
    requested: bool,
    guard: Option<ActivityGuard>,
    acquisition: crate::effects::EffectHandle<Task<()>>,
    acquire: AcquirePower,
}

impl Default for PlaybackPower {
    fn default() -> Self {
        Self {
            requested: false,
            guard: None,
            acquisition: Default::default(),
            acquire: crate::player::power::request,
        }
    }
}

impl PlaybackPower {
    pub(super) fn stop(&mut self) {
        self.requested = false;
        self.acquisition.cancel();
        self.guard = None;
    }

    #[cfg(test)]
    pub(super) fn for_tests() -> Self {
        Self {
            acquire: |_, cx| cx.prevent_idle_sleep(crate::player::power::PLAYBACK_POWER_REASON),
            ..Default::default()
        }
    }
}

impl PlaybackPage {
    fn should_inhibit_idle(&self) -> bool {
        self.session.should_inhibit_idle(
            self.video.has_backend(),
            self.presentation.frame.source_size.is_some(),
        )
    }

    pub(super) fn release_power_if_idle(&mut self) {
        if !self.should_inhibit_idle() {
            self.power.stop();
        }
    }

    pub(super) fn sync_playback_power(&mut self, window: &Window, cx: &mut Context<Self>) {
        let desired = self.should_inhibit_idle();
        if self.power.requested == desired {
            return;
        }
        self.power.stop();
        if !desired {
            return;
        }
        // Latch the playing interval, including a failed request, so rendering
        // and position events cannot repeatedly call an unavailable system API.
        self.power.requested = true;
        let request = (self.power.acquire)(window, cx);
        self.power.acquisition.replace(cx.spawn(async move |page, cx| {
            let result = request.await;
            page.update(cx, |page, _| {
                if !page.should_inhibit_idle() {
                    page.power.stop();
                    return;
                }
                match result {
                    Ok(guard) => page.power.guard = Some(guard),
                    Err(error) => {
                        tracing::warn!(%error, "Could not inhibit locking and sleep during video playback");
                    }
                }
            })
            .ok();
        }));
    }
}

#[cfg(test)]
mod tests;
