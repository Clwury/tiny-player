use super::*;

pub(super) const PLAYBACK_PROGRESS_BAR_BOTTOM_OFFSET_PX: f32 = 24.0;
pub(super) const PLAYBACK_PROGRESS_BAR_HEIGHT_PX: f32 = 108.0;
pub(super) const PLAYBACK_PROGRESS_BAR_WIDTH_FRACTION: f32 = 0.4;
const PLAYBACK_PROGRESS_BAR_MIN_WIDTH_PX: f32 = 480.0;
pub(super) const PLAYBACK_BACK_BUTTON_OFFSET_PX: f32 = 16.0;
pub(super) const PLAYBACK_BACK_BUTTON_SIZE_PX: f32 = 32.0;
const FULLSCREEN_CONTROLS_HIDE_DELAY: Duration = Duration::from_secs(1);
const FULLSCREEN_CONTROLS_HOT_ZONE_FRACTION: f32 = 0.5;

impl PlaybackPage {
    pub(super) fn progress_bar_visible(&self) -> bool {
        !self.presentation.episode_list.open
            && playback_progress_bar_visible(
                self.session.timeline().duration.is_some(),
                self.presentation.fullscreen.controls_visible,
            )
    }

    pub(super) fn reset_fullscreen_controls(&mut self) {
        self.presentation.window_drag = WindowDragState::Idle;
        self.presentation.fullscreen.cursor_visible = false;
        self.presentation.fullscreen.controls_visible = false;
        self.presentation.fullscreen.mouse_in_controls = false;
        self.presentation.fullscreen.mouse_in_back_button = false;
        self.presentation
            .timeline_presentation
            .progress_hover_cursor = None;
        self.presentation
            .presentation_timers
            .cancel(PresentationTimer::Controls);
        self.presentation.track_select_open = None;
        self.presentation.episode_list.open = false;
    }

    pub(super) fn schedule_fullscreen_controls_hide(&mut self, cx: &mut Context<Self>) {
        self.schedule_presentation_timer(
            PresentationTimer::Controls,
            FULLSCREEN_CONTROLS_HIDE_DELAY,
            cx,
        );
    }

    pub(super) fn hide_idle_fullscreen_controls(&mut self, cx: &mut Context<Self>) {
        if self.presentation.episode_list.open
            || !playback_controls_should_hide(
                self.presentation.fullscreen.mouse_in_controls,
                self.presentation.fullscreen.mouse_in_back_button,
                self.session.timeline().progress_drag_position.is_some(),
            )
        {
            return;
        }

        let changed = self.presentation.fullscreen.cursor_visible
            || self.presentation.fullscreen.controls_visible
            || self.presentation.track_select_open.is_some();
        self.presentation.fullscreen.cursor_visible = false;
        self.presentation.fullscreen.controls_visible = false;
        self.presentation.track_select_open = None;
        self.presentation
            .timeline_presentation
            .progress_hover_cursor = None;
        if changed {
            cx.notify();
        }
    }

    pub(super) fn handle_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_fullscreen = window.is_fullscreen();
        let bounds = window_viewport_bounds(window);
        let in_controls = playback_controls_contains(event.position, bounds, is_fullscreen);
        let in_hot_zone =
            playback_controls_hot_zone_contains(event.position, bounds, is_fullscreen);

        let controls_visible =
            self.presentation.fullscreen.controls_visible || in_controls || in_hot_zone;
        let changed = !self.presentation.fullscreen.cursor_visible
            || self.presentation.fullscreen.controls_visible != controls_visible
            || self.presentation.fullscreen.mouse_in_controls != in_controls;

        self.presentation.fullscreen.cursor_visible = true;
        self.presentation.fullscreen.controls_visible = controls_visible;
        self.presentation.fullscreen.mouse_in_controls = in_controls;
        self.schedule_fullscreen_controls_hide(cx);

        if changed {
            cx.notify();
        }
    }

    pub(super) fn handle_back_button_hover(
        &mut self,
        hovered: &bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if *hovered {
            let changed = !self.presentation.fullscreen.cursor_visible
                || !self.presentation.fullscreen.controls_visible
                || !self.presentation.fullscreen.mouse_in_back_button;
            self.presentation
                .presentation_timers
                .cancel(PresentationTimer::Controls);
            self.presentation.fullscreen.cursor_visible = true;
            self.presentation.fullscreen.controls_visible = true;
            self.presentation.fullscreen.mouse_in_back_button = true;
            if changed {
                cx.notify();
            }
            return;
        }

        self.presentation.fullscreen.mouse_in_back_button = false;
        self.schedule_fullscreen_controls_hide(cx);
    }

    pub(super) fn handle_back_button_mouse_move(
        &mut self,
        _: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if !self.presentation.fullscreen.mouse_in_back_button
            || !self.presentation.fullscreen.controls_visible
            || !self.presentation.fullscreen.cursor_visible
        {
            self.presentation
                .presentation_timers
                .cancel(PresentationTimer::Controls);
            self.presentation.fullscreen.mouse_in_back_button = true;
            self.presentation.fullscreen.controls_visible = true;
            self.presentation.fullscreen.cursor_visible = true;
            cx.notify();
        }
    }
}

fn playback_controls_should_hide(
    mouse_in_controls: bool,
    mouse_in_back_button: bool,
    progress_dragging: bool,
) -> bool {
    !mouse_in_controls && !mouse_in_back_button && !progress_dragging
}

pub(super) fn window_viewport_bounds(window: &Window) -> Bounds<Pixels> {
    Bounds::new(gpui::point(px(0.0), px(0.0)), window.viewport_size())
}

pub(super) fn playback_progress_bar_visible(has_duration: bool, controls_visible: bool) -> bool {
    has_duration && controls_visible
}

pub(super) fn playback_back_button_visible(is_fullscreen: bool, controls_visible: bool) -> bool {
    !is_fullscreen && controls_visible
}

pub(super) fn playback_controls_hot_zone_contains(
    position: Point<Pixels>,
    viewport_bounds: Bounds<Pixels>,
    is_fullscreen: bool,
) -> bool {
    if is_fullscreen {
        position.y
            >= viewport_bounds.origin.y
                + viewport_bounds.size.height * FULLSCREEN_CONTROLS_HOT_ZONE_FRACTION
    } else {
        viewport_bounds.contains(&position)
    }
}

pub(super) fn playback_controls_contains(
    position: Point<Pixels>,
    viewport_bounds: Bounds<Pixels>,
    is_fullscreen: bool,
) -> bool {
    playback_progress_bar_bounds(viewport_bounds).contains(&position)
        || (!is_fullscreen && playback_back_button_bounds(viewport_bounds).contains(&position))
}

pub(super) fn playback_back_button_bounds(viewport_bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds::new(
        gpui::point(
            viewport_bounds.origin.x + px(PLAYBACK_BACK_BUTTON_OFFSET_PX),
            viewport_bounds.origin.y + px(PLAYBACK_BACK_BUTTON_OFFSET_PX),
        ),
        gpui::size(
            px(PLAYBACK_BACK_BUTTON_SIZE_PX),
            px(PLAYBACK_BACK_BUTTON_SIZE_PX),
        ),
    )
}

pub(super) fn playback_progress_bar_bounds(viewport_bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    let width = (viewport_bounds.size.width * PLAYBACK_PROGRESS_BAR_WIDTH_FRACTION)
        .max(px(PLAYBACK_PROGRESS_BAR_MIN_WIDTH_PX))
        .min((viewport_bounds.size.width - px(32.0)).max(px(0.0)));
    Bounds::new(
        gpui::point(
            viewport_bounds.origin.x + (viewport_bounds.size.width - width) / 2.0,
            viewport_bounds.origin.y + viewport_bounds.size.height
                - px(PLAYBACK_PROGRESS_BAR_BOTTOM_OFFSET_PX + PLAYBACK_PROGRESS_BAR_HEIGHT_PX),
        ),
        gpui::size(width, px(PLAYBACK_PROGRESS_BAR_HEIGHT_PX)),
    )
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, point, px, size};

    use super::*;

    #[test]
    fn playback_progress_bar_is_gated_by_duration_and_controls_visibility() {
        assert!(!playback_progress_bar_visible(false, false));
        assert!(!playback_progress_bar_visible(false, true));
        assert!(!playback_progress_bar_visible(true, false));
        assert!(playback_progress_bar_visible(true, true));
    }

    #[test]
    fn windowed_back_button_follows_controls_visibility() {
        assert!(!playback_back_button_visible(false, false));
        assert!(playback_back_button_visible(false, true));
        assert!(!playback_back_button_visible(true, false));
        assert!(!playback_back_button_visible(true, true));
    }

    #[test]
    fn hovered_back_button_blocks_scheduled_controls_hide() {
        assert!(!playback_controls_should_hide(false, true, false));
        assert!(!playback_controls_should_hide(true, false, false));
        assert!(playback_controls_should_hide(false, false, false));
        assert!(!playback_controls_should_hide(false, false, true));
    }

    #[test]
    fn playback_controls_hot_zone_uses_full_window_only_when_windowed() {
        let viewport = Bounds::new(point(px(0.0), px(100.0)), size(px(800.0), px(600.0)));

        assert!(!playback_controls_hot_zone_contains(
            point(px(400.0), px(399.0)),
            viewport,
            true,
        ));
        assert!(playback_controls_hot_zone_contains(
            point(px(400.0), px(400.0)),
            viewport,
            true,
        ));
        assert!(playback_controls_hot_zone_contains(
            point(px(400.0), px(101.0)),
            viewport,
            false,
        ));
        assert!(!playback_controls_hot_zone_contains(
            point(px(400.0), px(99.0)),
            viewport,
            false,
        ));
    }

    #[test]
    fn playback_controls_hit_area_matches_visible_controls() {
        let viewport = Bounds::new(point(px(0.0), px(0.0)), size(px(1000.0), px(1000.0)));

        assert_eq!(
            playback_progress_bar_bounds(viewport),
            Bounds::new(point(px(260.0), px(868.0)), size(px(480.0), px(108.0)))
        );
        assert!(playback_controls_contains(
            point(px(500.0), px(900.0)),
            viewport,
            true,
        ));
        assert!(!playback_controls_contains(
            point(px(500.0), px(800.0)),
            viewport,
            true,
        ));
        assert!(!playback_controls_contains(
            point(px(750.0), px(900.0)),
            viewport,
            true,
        ));
        assert!(!playback_controls_contains(
            point(px(32.0), px(32.0)),
            viewport,
            true,
        ));
        assert!(playback_controls_contains(
            point(px(32.0), px(32.0)),
            viewport,
            false,
        ));
    }
}
