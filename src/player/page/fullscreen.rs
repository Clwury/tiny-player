use super::*;

pub(super) const PLAYBACK_PROGRESS_BAR_BOTTOM_OFFSET_PX: f32 = 24.0;
pub(super) const PLAYBACK_PROGRESS_BAR_HEIGHT_PX: f32 = 94.0;
pub(super) const PLAYBACK_BACK_BUTTON_OFFSET_PX: f32 = 16.0;
pub(super) const PLAYBACK_BACK_BUTTON_SIZE_PX: f32 = 32.0;
const FULLSCREEN_CONTROLS_HIDE_DELAY: Duration = Duration::from_millis(500);
const FULLSCREEN_CONTROLS_HOT_ZONE_FRACTION: f32 = 0.5;

impl PlaybackPage {
    pub(super) fn progress_bar_visible(&self) -> bool {
        playback_progress_bar_visible(
            self.timeline.duration.is_some(),
            self.fullscreen.controls_visible,
        )
    }

    pub(super) fn reset_fullscreen_controls(&mut self) {
        self.fullscreen.cursor_visible = false;
        self.fullscreen.controls_visible = false;
        self.fullscreen.mouse_in_controls = false;
        self.fullscreen.mouse_in_back_button = false;
        self.fullscreen.hide_generation = self.fullscreen.hide_generation.wrapping_add(1);
        self.tracks.open = None;
    }

    pub(super) fn schedule_fullscreen_controls_hide(&mut self, cx: &mut Context<Self>) {
        self.fullscreen.hide_generation = self.fullscreen.hide_generation.wrapping_add(1);
        let generation = self.fullscreen.hide_generation;

        cx.spawn(async move |page, cx| {
            Timer::after(FULLSCREEN_CONTROLS_HIDE_DELAY).await;
            page.update(cx, |page, cx| {
                page.hide_idle_fullscreen_controls(generation, cx);
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn hide_idle_fullscreen_controls(
        &mut self,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        if !playback_controls_should_hide(
            self.fullscreen.hide_generation,
            generation,
            self.fullscreen.mouse_in_controls,
            self.fullscreen.mouse_in_back_button,
            self.timeline.progress_drag_position.is_some(),
        ) {
            return;
        }

        let changed = self.fullscreen.cursor_visible
            || self.fullscreen.controls_visible
            || self.tracks.open.is_some();
        self.fullscreen.cursor_visible = false;
        self.fullscreen.controls_visible = false;
        self.tracks.open = None;
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

        let controls_visible = self.fullscreen.controls_visible || in_controls || in_hot_zone;
        let changed = !self.fullscreen.cursor_visible
            || self.fullscreen.controls_visible != controls_visible
            || self.fullscreen.mouse_in_controls != in_controls;

        self.fullscreen.cursor_visible = true;
        self.fullscreen.controls_visible = controls_visible;
        self.fullscreen.mouse_in_controls = in_controls;
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
            let changed = !self.fullscreen.cursor_visible
                || !self.fullscreen.controls_visible
                || !self.fullscreen.mouse_in_back_button;
            self.fullscreen.hide_generation = self.fullscreen.hide_generation.wrapping_add(1);
            self.fullscreen.cursor_visible = true;
            self.fullscreen.controls_visible = true;
            self.fullscreen.mouse_in_back_button = true;
            if changed {
                cx.notify();
            }
            return;
        }

        self.fullscreen.mouse_in_back_button = false;
        self.schedule_fullscreen_controls_hide(cx);
    }

    pub(super) fn handle_back_button_mouse_move(
        &mut self,
        _: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if !self.fullscreen.mouse_in_back_button
            || !self.fullscreen.controls_visible
            || !self.fullscreen.cursor_visible
        {
            self.fullscreen.hide_generation = self.fullscreen.hide_generation.wrapping_add(1);
            self.fullscreen.mouse_in_back_button = true;
            self.fullscreen.controls_visible = true;
            self.fullscreen.cursor_visible = true;
            cx.notify();
        }
    }
}

fn playback_controls_should_hide(
    current_generation: u64,
    requested_generation: u64,
    mouse_in_controls: bool,
    mouse_in_back_button: bool,
    progress_dragging: bool,
) -> bool {
    current_generation == requested_generation
        && !mouse_in_controls
        && !mouse_in_back_button
        && !progress_dragging
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
    Bounds::new(
        gpui::point(
            viewport_bounds.origin.x + viewport_bounds.size.width * 0.3,
            viewport_bounds.origin.y + viewport_bounds.size.height
                - px(PLAYBACK_PROGRESS_BAR_BOTTOM_OFFSET_PX + PLAYBACK_PROGRESS_BAR_HEIGHT_PX),
        ),
        gpui::size(
            viewport_bounds.size.width * 0.4,
            px(PLAYBACK_PROGRESS_BAR_HEIGHT_PX),
        ),
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
        assert!(!playback_controls_should_hide(7, 7, false, true, false));
        assert!(!playback_controls_should_hide(7, 7, true, false, false));
        assert!(playback_controls_should_hide(7, 7, false, false, false));
        assert!(!playback_controls_should_hide(8, 7, false, false, false));
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
            Bounds::new(point(px(300.0), px(882.0)), size(px(400.0), px(94.0)))
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
