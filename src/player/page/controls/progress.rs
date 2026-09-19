use super::*;

const PROGRESS_HOVER_LABEL_WIDTH_PX: f32 = 72.0;
const PROGRESS_HOVER_CURSOR_GAP_PX: f32 = 8.0;

struct ProgressHoverPreview {
    label: String,
    left: Pixels,
    width: Pixels,
    show_on_left: bool,
}

fn progress_hover_preview(timeline: &PlaybackTimelineState) -> Option<ProgressHoverPreview> {
    let duration = timeline.duration.and_then(valid_playback_duration)?;
    let bounds = timeline.progress_track_bounds?;
    let cursor = timeline.progress_hover_cursor?;
    if !bounds.contains(&cursor) {
        return None;
    }
    let fraction = progress_fraction_for_cursor(cursor.x, bounds)?;
    let cursor_offset = cursor.x - bounds.left();
    let show_on_left = fraction > 0.5;
    let available_width = if show_on_left {
        cursor_offset
    } else {
        bounds.size.width - cursor_offset
    };
    let gap = px(PROGRESS_HOVER_CURSOR_GAP_PX).min(available_width);
    let width = px(PROGRESS_HOVER_LABEL_WIDTH_PX).min(available_width - gap);
    let left = if show_on_left {
        cursor_offset - gap - width
    } else {
        cursor_offset + gap
    };

    Some(ProgressHoverPreview {
        label: format_playback_time(duration * f64::from(fraction)),
        left,
        width,
        show_on_left,
    })
}

pub(super) fn render_progress_hover(
    timeline: &PlaybackTimelineState,
    cx: &gpui::App,
) -> Option<impl IntoElement> {
    let preview = progress_hover_preview(timeline)?;
    Some(
        div()
            .debug_selector(|| "playback-progress-hover-time".to_string())
            .absolute()
            .top(px(28.0))
            .left(preview.left)
            .w(preview.width)
            .h(px(20.0))
            .flex()
            .items_center()
            .when(preview.show_on_left, |label| label.justify_end())
            .when(!preview.show_on_left, |label| label.justify_start())
            .text_xs()
            .text_color(theme::media_overlay(cx).foreground)
            .whitespace_nowrap()
            .overflow_hidden()
            .child(preview.label),
    )
}

impl PlaybackPage {
    pub(in super::super) fn update_progress_hover(
        &mut self,
        cursor: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        if self.timeline.progress_hover_cursor != cursor {
            self.timeline.progress_hover_cursor = cursor;
            cx.notify();
        }
    }

    pub(in super::super) fn begin_progress_drag(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.close_track_select(cx) {
            cx.stop_propagation();
            return;
        }
        self.update_progress_drag(event.position.x, cx);
        cx.stop_propagation();
    }

    pub(in super::super) fn drag_progress(
        &mut self,
        event: &DragMoveEvent<ProgressBarDrag>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.update_progress_hover(Some(event.event.position), cx);
        self.update_progress_drag(event.event.position.x, cx);
        cx.stop_propagation();
    }

    pub(in super::super) fn finish_progress_drag(
        &mut self,
        _event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The outside-release listener runs even for clicks on the titlebar.
        // Only consume a release belonging to an active seek; Windows caption
        // buttons need their mouse-up event to reach the native backend.
        if self.timeline.progress_drag_position.is_none() {
            return;
        }
        self.commit_progress_drag(window, cx);
        cx.stop_propagation();
    }

    pub(in super::super) fn update_progress_drag(
        &mut self,
        cursor_x: Pixels,
        cx: &mut Context<Self>,
    ) {
        let Some(position) = self.position_for_progress_cursor(cursor_x) else {
            return;
        };
        if self
            .timeline
            .progress_drag_position
            .is_none_or(|current| (current - position).abs() >= 0.02)
        {
            self.timeline.progress_drag_position = Some(position);
            cx.notify();
        }
    }

    pub(in super::super) fn commit_progress_drag(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(position) = self.timeline.progress_drag_position else {
            return;
        };
        self.seek_to_position(position, window, cx);
    }

    pub(in super::super) fn position_for_progress_cursor(&self, cursor_x: Pixels) -> Option<f64> {
        let duration = self.timeline.duration?;
        let bounds = self.timeline.progress_track_bounds?;
        let fraction = progress_fraction_for_cursor(cursor_x, bounds)?;
        Some(clamp_playback_position(
            duration * fraction as f64,
            duration,
        ))
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, point, size};

    use crate::player::backend::{DemuxCacheState, PlaybackCacheTimeRange};

    use super::*;

    #[gpui::test]
    fn forward_cache_remains_drawn_when_seekable_start_passes_the_playhead(
        cx: &mut TestAppContext,
    ) {
        struct ProgressPreview {
            timeline: PlaybackTimelineState,
        }

        impl Render for ProgressPreview {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let theme = theme::media_overlay(cx);
                div()
                    .relative()
                    .w(px(400.0))
                    .h(px(28.0))
                    .child(progress_track_fill(theme.input_border, 1.0))
                    .children(progress_track_forward_cache_fill(
                        theme.input_border_focused.opacity(0.35),
                        forward_cache_fraction(&self.timeline),
                    ))
                    .child(progress_track_played_fill(
                        theme.input_border_focused,
                        progress_fraction(
                            self.timeline.position.unwrap(),
                            self.timeline.duration.unwrap(),
                        ),
                    ))
            }
        }

        cx.update(theme::init);
        let (view, cx) = cx.add_window_view(|_, _| ProgressPreview {
            timeline: PlaybackTimelineState {
                position: Some(82.8),
                duration: Some(200.0),
                buffered_until: Some(127.6),
                cache_state: Some(PlaybackCacheState {
                    demux: DemuxCacheState {
                        seekable_ranges: vec![PlaybackCacheTimeRange {
                            start: 69.666666667,
                            end: 124.6,
                        }],
                        ..DemuxCacheState::default()
                    },
                    ..PlaybackCacheState::default()
                }),
                ..PlaybackTimelineState::default()
            },
        });
        cx.run_until_parked();
        let before = cx.debug_bounds("playback-progress-forward-cache").unwrap();
        assert!((f32::from(before.size.width) - 255.2).abs() <= 1.0);

        view.update(cx, |view, cx| {
            view.timeline
                .cache_state
                .as_mut()
                .unwrap()
                .demux
                .seekable_ranges[0]
                .start = 84.3;
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            cx.debug_bounds("playback-progress-forward-cache"),
            Some(before)
        );

        view.update(cx, |view, cx| {
            view.timeline.position = Some(84.4);
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            cx.debug_bounds("playback-progress-forward-cache"),
            Some(before)
        );

        view.update(cx, |view, cx| {
            view.timeline.buffered_until = None;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("playback-progress-forward-cache").is_none());
    }

    #[test]
    fn progress_hover_time_follows_the_cursor_and_formats_hours() {
        let mut timeline = PlaybackTimelineState {
            duration: Some(7322.0),
            progress_track_bounds: Some(Bounds::new(
                point(px(100.0), px(200.0)),
                size(px(400.0), px(28.0)),
            )),
            ..PlaybackTimelineState::default()
        };
        // GPUI hit testing excludes the exact right edge.
        for (cursor_x, expected) in [(100.0, "0:00"), (300.0, "1:01:01"), (499.999, "2:02:02")] {
            timeline.progress_hover_cursor = Some(point(px(cursor_x), px(214.0)));
            assert_eq!(progress_hover_preview(&timeline).unwrap().label, expected);
        }
        timeline.duration = Some(1800.0);
        timeline.progress_hover_cursor = Some(point(px(300.0), px(214.0)));
        assert_eq!(progress_hover_preview(&timeline).unwrap().label, "15:00");

        // Recompute against the resized track without waiting for another mouse move.
        timeline.progress_track_bounds.as_mut().unwrap().size.width = px(800.0);
        assert_eq!(progress_hover_preview(&timeline).unwrap().label, "7:30");
    }

    #[test]
    fn progress_hover_disappears_outside_the_track_or_without_valid_duration() {
        let mut timeline = PlaybackTimelineState {
            duration: Some(120.0),
            progress_track_bounds: Some(Bounds::new(
                point(px(100.0), px(200.0)),
                size(px(400.0), px(28.0)),
            )),
            ..PlaybackTimelineState::default()
        };
        for cursor in [
            None,
            Some(point(px(99.0), px(214.0))),
            Some(point(px(501.0), px(214.0))),
            Some(point(px(300.0), px(199.0))),
            Some(point(px(300.0), px(229.0))),
        ] {
            timeline.progress_hover_cursor = cursor;
            assert!(progress_hover_preview(&timeline).is_none());
        }
        timeline.progress_hover_cursor = Some(point(px(100.0), px(214.0)));
        for duration in [
            None,
            Some(0.0),
            Some(-1.0),
            Some(f64::NAN),
            Some(f64::INFINITY),
        ] {
            timeline.duration = duration;
            assert!(progress_hover_preview(&timeline).is_none());
        }
        timeline.duration = Some(120.0);
        timeline.progress_track_bounds.as_mut().unwrap().size.width = px(0.0);
        assert!(progress_hover_preview(&timeline).is_none());
    }

    #[gpui::test]
    fn progress_hover_label_switches_cursor_sides_after_the_midpoint(cx: &mut TestAppContext) {
        struct ProgressPreview {
            timeline: PlaybackTimelineState,
        }

        impl Render for ProgressPreview {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let bounds = self.timeline.progress_track_bounds.unwrap();
                div().relative().size_full().child(
                    div()
                        .debug_selector(|| "preview-track".to_string())
                        .absolute()
                        .left(bounds.left())
                        .top(bounds.top())
                        .w(bounds.size.width)
                        .h(bounds.size.height)
                        .child(progress_track_fill(
                            theme::media_overlay(cx).input_border,
                            1.0,
                        ))
                        .when_some(
                            render_progress_hover(&self.timeline, cx),
                            |track, preview| track.child(preview),
                        ),
                )
            }
        }

        cx.update(theme::init);
        let (view, cx) = cx.add_window_view(|_, _| ProgressPreview {
            timeline: PlaybackTimelineState {
                duration: Some(7322.0),
                progress_track_bounds: Some(Bounds::new(
                    point(px(16.0), px(16.0)),
                    size(px(400.0), px(28.0)),
                )),
                ..PlaybackTimelineState::default()
            },
        });
        for width in [48.0, 160.0, 400.0] {
            for fraction in [0.0, 0.4999, 0.5, 0.5001, 1.0] {
                view.update(cx, |view, cx| {
                    view.timeline
                        .progress_track_bounds
                        .as_mut()
                        .unwrap()
                        .size
                        .width = px(width);
                    let offset = (width * fraction).min(width - 0.001);
                    view.timeline.progress_hover_cursor = Some(point(px(16.0 + offset), px(30.0)));
                    cx.notify();
                });
                cx.run_until_parked();
                let track = cx.debug_bounds("preview-track").unwrap();
                let label = cx.debug_bounds("playback-progress-hover-time").unwrap();
                assert_eq!(label.top(), track.bottom());
                assert!(label.left() >= track.left());
                assert!(label.right() <= track.right());
                let cursor =
                    view.read_with(cx, |view, _| view.timeline.progress_hover_cursor.unwrap());
                // GPUI rounds painted bounds to physical pixels.
                if fraction <= 0.5 {
                    assert!(
                        f32::from(label.left() - cursor.x - px(PROGRESS_HOVER_CURSOR_GAP_PX)).abs()
                            <= 1.0
                    );
                } else {
                    assert!(
                        f32::from(label.right() - cursor.x + px(PROGRESS_HOVER_CURSOR_GAP_PX))
                            .abs()
                            <= 1.0
                    );
                }
            }
        }
        view.update(cx, |view, cx| {
            view.timeline.progress_hover_cursor = None;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("playback-progress-hover-time").is_none());
    }
}
