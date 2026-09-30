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

    pub(super) fn render_playback_rate_indicator(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = theme::get(cx);
        div()
            .id("playback-rate-indicator")
            .absolute()
            .left(px(76.0))
            .top(px(fullscreen::PLAYBACK_BACK_BUTTON_OFFSET_PX))
            .h(px(fullscreen::PLAYBACK_BACK_BUTTON_SIZE_PX))
            .flex()
            .items_center()
            .text_sm()
            .text_color(theme.accent)
            .child(format!("{:.2}×", self.session.controls_view().rate))
    }
}
