//! GPUI timer and effect adapter for the pure reporting controller.
mod delivery;
use super::*;
use crate::{
    effects::EffectHandle,
    player::reporting::{
        PROGRESS_INTERVAL, ReportingIntent, ReportingTransition, effect::PlaybackReporter,
    },
};

// Page-owned timer is cancelled at close/release. Reporter drains the terminal
// command against its immutable account snapshot; failures only update receipt.
pub(super) struct ReportingEffects {
    reporter: delivery::ReportDelivery,
    periodic: EffectHandle<gpui::Task<()>>,
}

impl ReportingEffects {
    pub(super) fn new(
        gateway: Arc<dyn crate::player::reporting::gateway::PlaybackReportGateway>,
        identity: crate::effects::WorkspaceIdentity,
        cx: &gpui::App,
    ) -> Self {
        Self {
            reporter: delivery::ReportDelivery::new(
                PlaybackReporter::new(gateway, identity),
                cx.foreground_executor().clone(),
            ),
            periodic: EffectHandle::default(),
        }
    }
}

impl PlaybackPage {
    fn dispatch_reporting(&mut self, intent: ReportingIntent) -> ReportingTransition {
        self.session.report(
            intent,
            &crate::player::session::BackendContext {
                item_id: &self.emby.item_id,
                media_source_id: &self.emby.media_source_id,
                play_session_id: self.emby.play_session_id.as_deref(),
                run_time_ticks: self.emby.run_time_ticks,
                has_frame: self.presentation.frame.current.is_some(),
            },
        )
    }

    #[cfg(test)]
    pub(super) fn handle_playback_restart_reporting(&mut self, cx: &mut Context<Self>) {
        let transition = self.dispatch_reporting(ReportingIntent::Restart);
        self.execute_reporting_transition(transition, cx);
    }

    pub(super) fn execute_reporting_transition(
        &mut self,
        transition: ReportingTransition,
        cx: &mut Context<Self>,
    ) -> Option<PlaybackStateUpdate> {
        if transition.started && self.session.take_start_subtitle_preference() {
            self.remember_track_choice(PlaybackTrackKind::Subtitle, cx);
        }
        if let Some(command) = transition.command {
            self.report_effects.reporter.send(command);
        }
        if self.session.reporting.is_closed() {
            self.report_effects.periodic.cancel();
        } else if transition.started {
            self.schedule_periodic_playback_progress(cx);
        }
        transition.update
    }

    pub(super) fn report_playback_progress(&mut self, force: bool) {
        let transition = self.dispatch_reporting(ReportingIntent::Progress { force });
        if let Some(command) = transition.command {
            self.report_effects.reporter.send(command);
        }
    }

    pub(super) fn close_playback_reporting(
        &mut self,
        failed: bool,
        ended: bool,
    ) -> PlaybackStateUpdate {
        self.power.stop();
        let transition = self.dispatch_reporting(ReportingIntent::Close { failed, ended });
        self.report_effects.periodic.cancel();
        if let Some(command) = transition.command {
            self.report_effects.reporter.send(command);
        }
        transition
            .update
            .expect("closing a reporting session returns its final state")
    }

    pub(super) fn playback_reporting_closed(&self) -> bool {
        self.session.reporting.is_closed()
    }

    pub(super) fn schedule_periodic_playback_progress(&mut self, cx: &mut Context<Self>) {
        let Some(token) = self.session.reporting.begin_periodic() else {
            return;
        };
        self.report_effects
            .periodic
            .replace(cx.spawn(async move |page, cx| {
                cx.background_executor().timer(PROGRESS_INTERVAL).await;
                page.update(cx, |page, cx| {
                    if !page
                        .session
                        .reporting
                        .accept_periodic(&token, &page.emby.server.workspace_identity())
                    {
                        return;
                    }
                    page.report_playback_progress(false);
                    page.schedule_periodic_playback_progress(cx);
                })
                .ok();
            }));
    }
}

impl Drop for PlaybackPage {
    fn drop(&mut self) {
        if !self.playback_reporting_closed() {
            let _ = self.close_playback_reporting(false, self.session.timeline().ended);
        }
    }
}

#[cfg(test)]
mod tests;
