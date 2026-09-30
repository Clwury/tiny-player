use super::*;
use crate::effects::{EffectHandle, RequestToken, WorkspaceIdentity};
pub(super) use crate::player::model::timers::{PresentationTimer, PresentationTimers};

// Page-owned GPUI continuations, at most one per typed deadline. Replacing,
// canceling, returning, or dropping releases tasks. Stale callbacks do nothing;
// these local timers have no IO failures or user-facing error notification key.
pub(super) struct PresentationEffects {
    model: PresentationTimers,
    tasks: [EffectHandle<gpui::Task<()>>; 4],
}

impl PresentationEffects {
    pub(super) fn new(identity: WorkspaceIdentity) -> Self {
        Self {
            model: PresentationTimers::new(identity),
            tasks: Default::default(),
        }
    }

    pub(super) fn cancel(&mut self, kind: PresentationTimer) {
        self.model.cancel(kind);
        self.tasks[kind.index()].cancel();
    }

    pub(super) fn close(&mut self) {
        self.model.close();
        for task in &mut self.tasks {
            task.cancel();
        }
    }
}

impl PlaybackPage {
    pub(super) fn schedule_presentation_timer(
        &mut self,
        kind: PresentationTimer,
        delay: Duration,
        cx: &mut Context<Self>,
    ) {
        let Some(token) = self.presentation.presentation_timers.model.begin(kind) else {
            return;
        };
        self.presentation.presentation_timers.tasks[kind.index()].replace(cx.spawn(
            async move |page, cx| {
                cx.background_executor().timer(delay).await;
                page.update(cx, |page, cx| {
                    page.complete_presentation_timer(kind, &token, cx)
                })
                .ok();
            },
        ));
    }

    fn complete_presentation_timer(
        &mut self,
        kind: PresentationTimer,
        token: &RequestToken,
        cx: &mut Context<Self>,
    ) {
        if !self.presentation.presentation_timers.model.complete(
            kind,
            token,
            &self.emby.server.workspace_identity(),
        ) {
            return;
        }
        self.presentation.presentation_timers.tasks[kind.index()].cancel();
        let changed = match kind {
            PresentationTimer::Controls => {
                self.hide_idle_fullscreen_controls(cx);
                false // The controls handler checks visibility and notifies itself.
            }
            PresentationTimer::Volume => {
                std::mem::take(&mut self.presentation.volume_indicator_visible)
            }
            PresentationTimer::Rate => {
                std::mem::take(&mut self.presentation.rate_indicator_visible)
            }
            PresentationTimer::DownloadSpeed => self.progress_bar_visible(),
        };
        if changed {
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests;
