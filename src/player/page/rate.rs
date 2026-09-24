use super::*;
use tiny_playback::PlaybackRateChange;

pub(super) struct PlaybackRateState {
    pub(super) value: f64,
    pub(super) indicator_visible: bool,
    hide_generation: u64,
}

impl Default for PlaybackRateState {
    fn default() -> Self {
        Self {
            value: 1.0,
            indicator_visible: false,
            hide_generation: 0,
        }
    }
}

impl PlaybackPage {
    pub(super) fn change_playback_rate(
        &mut self,
        change: PlaybackRateChange,
        cx: &mut Context<Self>,
    ) {
        let next = change.apply(self.rate.value);
        let Some(backend) = self.video.owner_mut() else {
            return;
        };
        if let Err(error) = backend.command(BackendCommand::SetPlaybackRate { rate: next }) {
            self.error_message = Some(format!("调整播放速度失败：{error}").into());
            cx.notify();
            return;
        }
        self.rate.value = next;
        self.rate.indicator_visible = true;
        self.rate.hide_generation = self.rate.hide_generation.wrapping_add(1);
        let generation = self.rate.hide_generation;
        cx.spawn(async move |page, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1200))
                .await;
            page.update(cx, |page, cx| {
                if page.rate.hide_generation == generation {
                    page.rate.indicator_visible = false;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
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
            .child(format!("{:.2}×", self.rate.value))
    }
}
