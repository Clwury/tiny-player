use super::*;
use tiny_playback::PlaybackRateChange;

impl PlaybackPage {
    pub(super) fn change_playback_rate(
        &mut self,
        change: PlaybackRateChange,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_control(PlaybackIntent::ChangeRate(change), cx);
    }
}
