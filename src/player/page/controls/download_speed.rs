use std::time::Instant;

use gpui::FontWeight;

use super::*;

const DOWNLOAD_SPEED_UPDATE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(in super::super) struct DownloadSpeedDisplay {
    label: Option<String>,
    updated_at: Option<Instant>,
    refresh_scheduled: bool,
}

impl DownloadSpeedDisplay {
    fn update(&mut self, label: Option<String>, now: Instant) -> Option<Duration> {
        if self.label == label {
            return None;
        }
        if let Some(updated_at) = self.updated_at {
            let remaining = DOWNLOAD_SPEED_UPDATE_INTERVAL
                .saturating_sub(now.saturating_duration_since(updated_at));
            if !remaining.is_zero() {
                return Some(remaining);
            }
        }

        self.label = label;
        self.updated_at = Some(now);
        None
    }
}

impl PlaybackPage {
    pub(in super::super) fn update_download_speed(&mut self, cx: &mut Context<Self>) {
        let label = http_download_speed_label(
            self.source_protocol.as_deref(),
            self.timeline.cache_state.as_ref(),
        );
        let remaining = self
            .download_speed
            .update(label, cx.background_executor().now());
        if let Some(remaining) = remaining
            && !self.download_speed.refresh_scheduled
        {
            self.download_speed.refresh_scheduled = true;
            cx.spawn(async move |page, cx| {
                cx.background_executor().timer(remaining).await;
                page.update(cx, |page, cx| {
                    page.download_speed.refresh_scheduled = false;
                    if page.progress_bar_visible() {
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    pub(in super::super) fn render_download_speed(&self, cx: &Context<Self>) -> impl IntoElement {
        download_speed_indicator(self.download_speed.label.clone(), cx)
    }
}

fn http_download_speed_label(
    protocol: Option<&str>,
    cache_state: Option<&PlaybackCacheState>,
) -> Option<String> {
    if !matches!(protocol, Some("http" | "https")) {
        return None;
    }

    let speed = match cache_state.and_then(|state| state.byte.as_ref()) {
        Some(byte) => {
            let bytes_per_second = if byte.idle {
                0
            } else {
                byte.raw_input_rate.unwrap_or(0)
            };
            format!("{}/s", format_cache_bytes(bytes_per_second))
        }
        None => "—".to_string(),
    };
    Some(speed)
}

fn download_speed_indicator(label: Option<String>, cx: &gpui::App) -> impl IntoElement {
    let Some(label) = label else {
        return div().into_any_element();
    };
    let theme = theme::get(cx);

    div()
        .debug_selector(|| "playback-download-speed".to_string())
        .absolute()
        .right(px(PLAYBACK_BACK_BUTTON_OFFSET_PX))
        .top(px(PLAYBACK_BACK_BUTTON_OFFSET_PX))
        .h(px(PLAYBACK_BACK_BUTTON_SIZE_PX))
        .min_w(px(112.0))
        .flex()
        .items_center()
        .justify_center()
        .px_3()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.accent)
        .child(label)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, point, size};

    use tiny_playback::ByteCacheState;

    use super::*;

    #[test]
    fn download_speed_display_updates_at_most_once_per_second_with_the_latest_rate() {
        let start = Instant::now();
        let mut display = DownloadSpeedDisplay::default();
        assert_eq!(display.update(Some("1.0 MiB/s".into()), start), None);

        for (milliseconds, label) in [(100, "2.0 MiB/s"), (500, "3.0 MiB/s"), (999, "0 B/s")] {
            assert_eq!(
                display.update(
                    Some(label.into()),
                    start + Duration::from_millis(milliseconds)
                ),
                Some(Duration::from_millis(1000 - milliseconds))
            );
            assert_eq!(display.label.as_deref(), Some("1.0 MiB/s"));
        }

        assert_eq!(
            display.update(Some("0 B/s".into()), start + Duration::from_secs(1)),
            None
        );
        assert_eq!(display.label.as_deref(), Some("0 B/s"));
        assert_eq!(
            display.update(
                Some("4.0 MiB/s".into()),
                start + Duration::from_millis(1500)
            ),
            Some(Duration::from_millis(500))
        );
        assert_eq!(display.label.as_deref(), Some("0 B/s"));
    }

    #[test]
    fn unchanged_speed_does_not_postpone_the_next_update_after_controls_reappear() {
        let start = Instant::now();
        let mut display = DownloadSpeedDisplay::default();
        display.update(Some("—".into()), start);
        assert_eq!(
            display.update(Some("—".into()), start + Duration::from_millis(900)),
            None
        );
        assert_eq!(
            display.update(Some("2.0 MiB/s".into()), start + Duration::from_secs(5)),
            None
        );
        assert_eq!(display.label.as_deref(), Some("2.0 MiB/s"));
    }

    #[test]
    fn http_download_speed_uses_transport_rate_and_formats_units() {
        let mut state = PlaybackCacheState {
            byte: Some(ByteCacheState::default()),
            ..PlaybackCacheState::default()
        };
        state.demux.raw_input_rate = Some(99 * 1024 * 1024);
        for (rate, label) in [
            (0, "0 B/s"),
            (512, "512 B/s"),
            (1536, "1.5 KiB/s"),
            (2 * 1024 * 1024, "2.0 MiB/s"),
        ] {
            state.byte.as_mut().unwrap().raw_input_rate = Some(rate);
            for protocol in ["http", "https"] {
                assert_eq!(
                    http_download_speed_label(Some(protocol), Some(&state)).as_deref(),
                    Some(label)
                );
            }
        }
        for protocol in [None, Some("file"), Some("rtsp")] {
            assert_eq!(http_download_speed_label(protocol, Some(&state)), None);
        }
    }

    #[test]
    fn idle_http_download_reports_zero_and_missing_metrics_stay_unavailable() {
        let mut state = PlaybackCacheState::default();
        state.demux.raw_input_rate = Some(1024);
        assert_eq!(
            http_download_speed_label(Some("https"), Some(&state)).as_deref(),
            Some("—")
        );
        assert_eq!(
            http_download_speed_label(Some("https"), None).as_deref(),
            Some("—")
        );
        state.byte = Some(ByteCacheState {
            idle: true,
            raw_input_rate: Some(1024 * 1024),
            ..ByteCacheState::default()
        });
        assert_eq!(
            http_download_speed_label(Some("https"), Some(&state)).as_deref(),
            Some("0 B/s")
        );
        let byte = state.byte.as_mut().unwrap();
        byte.idle = false;
        byte.raw_input_rate = None;
        assert_eq!(
            http_download_speed_label(Some("https"), Some(&state)).as_deref(),
            Some("0 B/s")
        );
    }

    #[gpui::test]
    fn download_speed_stays_at_top_right_over_the_video_frame(cx: &mut TestAppContext) {
        struct SpeedPreview;

        impl Render for SpeedPreview {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                div()
                    .relative()
                    .size_full()
                    .overflow_hidden()
                    .child(div().size_full().bg(rgb(0x000000)))
                    .child(download_speed_indicator(Some("—".into()), cx))
            }
        }

        cx.update(theme::init);
        let (_, cx) = cx.add_window_view(|_, _| SpeedPreview);
        for selection in theme::ColorTheme::ALL {
            cx.update(|_, cx| theme::set(selection, cx));
            for (width, height) in [(640.0, 360.0), (1280.0, 720.0)] {
                cx.simulate_resize(size(px(width), px(height)));
                cx.run_until_parked();
                let bounds = cx.debug_bounds("playback-download-speed").unwrap();
                assert_eq!(bounds.top(), px(PLAYBACK_BACK_BUTTON_OFFSET_PX));
                assert_eq!(bounds.right(), px(width - PLAYBACK_BACK_BUTTON_OFFSET_PX));
                assert_eq!(bounds.size.height, px(PLAYBACK_BACK_BUTTON_SIZE_PX));
                assert_eq!(
                    bounds.center(),
                    point(px(width) - px(16.0) - bounds.size.width / 2.0, px(32.0))
                );
            }
        }
    }
}
