use std::time::Instant;

use gpui::FontWeight;

use super::*;

const DOWNLOAD_SPEED_UPDATE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(in super::super) struct DownloadSpeedDisplay {
    label: Option<String>,
    updated_at: Option<Instant>,
    native_input: Option<NativeInputSample>,
}

struct NativeInputSample {
    bytes: u64,
    at: Instant,
    rate: u64,
}

impl DownloadSpeedDisplay {
    fn update_for_source(
        &mut self,
        protocol: Option<&str>,
        cache_state: Option<&PlaybackCacheState>,
        now: Instant,
    ) -> Option<Duration> {
        let bytes = matches!(protocol, Some("http" | "https"))
            .then(|| cache_state.and_then(|state| state.demux.native_http_bytes_read))
            .flatten();
        let native_rate = self.sample_native_input(bytes, now);
        let label = match native_rate {
            Some((rate, _)) => Some(format!("{}/s", format_cache_bytes(rate))),
            None => http_download_speed_label(protocol, cache_state),
        };
        let label_deadline = self.update(label, now);
        // Native AVIO reports cumulative bytes. Sample once a second even
        // without new cache events, so a stopped/full download settles to 0.
        // The existing visibility-scoped timer stops when controls are hidden.
        label_deadline
            .into_iter()
            .chain(native_rate.map(|(_, remaining)| remaining))
            .min()
    }

    fn sample_native_input(&mut self, bytes: Option<u64>, now: Instant) -> Option<(u64, Duration)> {
        let Some(bytes) = bytes else {
            self.native_input = None;
            return None;
        };
        let sample = self.native_input.get_or_insert(NativeInputSample {
            bytes,
            at: now,
            rate: 0,
        });
        let elapsed = now.saturating_duration_since(sample.at);
        if bytes < sample.bytes || elapsed > DOWNLOAD_SPEED_UPDATE_INTERVAL * 2 {
            // A recreated AVIO context resets its counter. Reappearing controls
            // also need a fresh interval rather than an average of hidden time.
            *sample = NativeInputSample {
                bytes,
                at: now,
                rate: 0,
            };
        } else if elapsed >= DOWNLOAD_SPEED_UPDATE_INTERVAL {
            sample.rate = ((bytes - sample.bytes) as f64 / elapsed.as_secs_f64()).round() as u64;
            sample.bytes = bytes;
            sample.at = now;
        }
        Some((
            sample.rate,
            DOWNLOAD_SPEED_UPDATE_INTERVAL.saturating_sub(now.saturating_duration_since(sample.at)),
        ))
    }

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
        let remaining = self.presentation.download_speed.update_for_source(
            self.session.source_view().source_protocol.as_deref(),
            self.session.timeline().cache_state.as_ref(),
            cx.background_executor().now(),
        );
        if let Some(remaining) = remaining {
            self.schedule_presentation_timer(PresentationTimer::DownloadSpeed, remaining, cx);
        }
    }

    pub(in super::super) fn render_download_speed(&self, cx: &Context<Self>) -> Option<gpui::Div> {
        download_speed_indicator(self.presentation.download_speed.label.clone(), cx)
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

fn download_speed_indicator(label: Option<String>, cx: &gpui::App) -> Option<gpui::Div> {
    let label = label?;
    let theme = theme::get(cx);

    Some(
        div()
            .debug_selector(|| "playback-download-speed".to_string())
            .h(px(PLAYBACK_BACK_BUTTON_SIZE_PX))
            .min_w(px(112.0))
            .flex()
            .items_center()
            .justify_center()
            .px_3()
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.accent)
            .child(label),
    )
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
    fn native_http_without_byte_cache_uses_downloaded_bytes_and_settles_to_zero() {
        let start = Instant::now();
        let mut display = DownloadSpeedDisplay::default();
        let mut state = PlaybackCacheState::default();
        state.demux.native_http_bytes_read = Some(64 * 1024);
        state.demux.raw_input_rate = Some(99 * 1024 * 1024);
        assert_eq!(
            display.update_for_source(Some("https"), Some(&state), start),
            Some(Duration::from_secs(1))
        );
        assert_eq!(display.label.as_deref(), Some("0 B/s"));
        state.demux.native_http_bytes_read = Some(128 * 1024);
        display.update_for_source(Some("https"), Some(&state), start + Duration::from_secs(1));
        assert_eq!(display.label.as_deref(), Some("64.0 KiB/s"));
        display.update_for_source(Some("https"), Some(&state), start + Duration::from_secs(2));
        assert_eq!(display.label.as_deref(), Some("0 B/s"));
    }

    #[test]
    fn native_http_fallback_overrides_stopped_startup_cache_and_resets_on_counter_restart() {
        let start = Instant::now();
        let mut display = DownloadSpeedDisplay::default();
        let mut state = PlaybackCacheState {
            byte: Some(ByteCacheState {
                idle: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        state.demux.native_http_bytes_read = Some(100);
        display.update_for_source(Some("http"), Some(&state), start);
        state.demux.native_http_bytes_read = Some(100 + 1024);
        display.update_for_source(Some("http"), Some(&state), start + Duration::from_secs(1));
        assert_eq!(display.label.as_deref(), Some("1.0 KiB/s"));
        state.demux.native_http_bytes_read = Some(50);
        display.update_for_source(Some("http"), Some(&state), start + Duration::from_secs(2));
        assert_eq!(display.label.as_deref(), Some("0 B/s"));
        state.demux.native_http_bytes_read = Some(50 + 2048);
        display.update_for_source(Some("http"), Some(&state), start + Duration::from_secs(3));
        assert_eq!(display.label.as_deref(), Some("2.0 KiB/s"));
    }

    #[test]
    fn reappearing_native_speed_starts_a_fresh_sample_and_switching_to_cache_clears_it() {
        let start = Instant::now();
        let mut display = DownloadSpeedDisplay::default();
        let mut state = PlaybackCacheState::default();
        state.demux.native_http_bytes_read = Some(100);
        display.update_for_source(Some("https"), Some(&state), start);
        state.demux.native_http_bytes_read = Some(1024 * 1024);
        display.update_for_source(Some("https"), Some(&state), start + Duration::from_secs(10));
        assert_eq!(display.label.as_deref(), Some("0 B/s"));
        state.demux.native_http_bytes_read = None;
        state.byte = Some(ByteCacheState {
            raw_input_rate: Some(2048),
            ..Default::default()
        });
        display.update_for_source(Some("https"), Some(&state), start + Duration::from_secs(11));
        assert_eq!(display.label.as_deref(), Some("2.0 KiB/s"));
        assert!(display.native_input.is_none());
    }

    #[gpui::test]
    fn native_download_refreshes_then_shows_zero_without_another_backend_event(
        cx: &mut TestAppContext,
    ) {
        let (page, cx) = test_support::playback_window(cx);
        page.update(cx, |page, cx| {
            page.session.source_mut().source_protocol = Some("https".into());
            let mut state = PlaybackCacheState::default();
            state.demux.native_http_bytes_read = Some(1024);
            page.session.timeline_mut().cache_state = Some(state);
            page.update_download_speed(cx);
        });
        cx.run_until_parked();
        page.update(cx, |page, _| {
            page.session
                .timeline_mut()
                .cache_state
                .as_mut()
                .unwrap()
                .demux
                .native_http_bytes_read = Some(2048);
        });
        cx.executor().advance_clock(Duration::from_millis(1100));
        cx.run_until_parked();
        page.read_with(cx, |page, _| {
            assert_eq!(
                page.presentation.download_speed.label.as_deref(),
                Some("1.0 KiB/s")
            );
        });
        cx.executor().advance_clock(Duration::from_millis(1100));
        cx.run_until_parked();
        page.read_with(cx, |page, _| {
            assert_eq!(
                page.presentation.download_speed.label.as_deref(),
                Some("0 B/s")
            );
        });
    }

    #[gpui::test]
    fn page_refresh_uses_latest_rate_at_original_deadline_and_pauses_when_hidden(
        cx: &mut TestAppContext,
    ) {
        let (page, cx) = test_support::playback_window(cx);
        page.update(cx, |page, cx| {
            page.session.source_mut().source_protocol = Some("https".into());
            page.session.timeline_mut().cache_state = Some(PlaybackCacheState {
                byte: Some(ByteCacheState {
                    raw_input_rate: Some(1024),
                    ..Default::default()
                }),
                ..Default::default()
            });
            page.update_download_speed(cx);
            assert_eq!(
                page.presentation.download_speed.label.as_deref(),
                Some("1.0 KiB/s")
            );
        });
        for rate in 2..=10 {
            cx.executor().advance_clock(Duration::from_millis(100));
            page.update(cx, |page, cx| {
                page.session
                    .timeline_mut()
                    .cache_state
                    .as_mut()
                    .unwrap()
                    .byte
                    .as_mut()
                    .unwrap()
                    .raw_input_rate = Some(rate * 1024);
                page.update_download_speed(cx);
                assert_eq!(
                    page.presentation.download_speed.label.as_deref(),
                    Some("1.0 KiB/s")
                );
            });
            cx.run_until_parked();
        }
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        page.read_with(cx, |page, _| {
            assert_eq!(
                page.presentation.download_speed.label.as_deref(),
                Some("10.0 KiB/s")
            )
        });
        page.update(cx, |page, cx| {
            page.session
                .timeline_mut()
                .cache_state
                .as_mut()
                .unwrap()
                .byte
                .as_mut()
                .unwrap()
                .raw_input_rate = Some(20 * 1024);
            page.update_download_speed(cx);
            page.presentation.fullscreen.controls_visible = false;
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        page.read_with(cx, |page, _| {
            assert_eq!(
                page.presentation.download_speed.label.as_deref(),
                Some("10.0 KiB/s")
            )
        });
        page.update(cx, |page, cx| {
            page.presentation.fullscreen.controls_visible = true;
            cx.notify();
        });
        cx.run_until_parked();
        page.read_with(cx, |page, _| {
            assert_eq!(
                page.presentation.download_speed.label.as_deref(),
                Some("20.0 KiB/s")
            )
        });
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
                    .child(
                        div()
                            .absolute()
                            .right(px(PLAYBACK_BACK_BUTTON_OFFSET_PX))
                            .top(px(PLAYBACK_BACK_BUTTON_OFFSET_PX))
                            .children(download_speed_indicator(Some("—".into()), cx)),
                    )
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
