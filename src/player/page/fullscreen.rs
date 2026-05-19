use super::*;

pub(super) const PLAYBACK_PROGRESS_BAR_BOTTOM_OFFSET_PX: f32 = 24.0;
pub(super) const PLAYBACK_PROGRESS_BAR_HEIGHT_PX: f32 = 94.0;
const FULLSCREEN_CONTROLS_HIDE_DELAY: Duration = Duration::from_secs(1);
const FULLSCREEN_CONTROLS_HOT_ZONE_FRACTION: f32 = 0.5;

impl PlaybackPage {
    pub(super) fn progress_bar_visible(&self, is_fullscreen: bool) -> bool {
        self.playback_duration.is_some()
            && playback_progress_bar_visible(is_fullscreen, self.fullscreen_controls_visible)
    }

    pub(super) fn reset_fullscreen_controls(&mut self) {
        self.fullscreen_cursor_visible = false;
        self.fullscreen_controls_visible = false;
        self.fullscreen_mouse_in_controls = false;
        self.fullscreen_controls_hide_generation =
            self.fullscreen_controls_hide_generation.wrapping_add(1);
        self.open_track_select = None;
    }

    pub(super) fn schedule_fullscreen_controls_hide(&mut self, cx: &mut Context<Self>) {
        self.fullscreen_controls_hide_generation =
            self.fullscreen_controls_hide_generation.wrapping_add(1);
        let generation = self.fullscreen_controls_hide_generation;

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
        if self.fullscreen_controls_hide_generation != generation
            || self.fullscreen_mouse_in_controls
            || self.progress_drag_position.is_some()
        {
            return;
        }

        let changed = self.fullscreen_cursor_visible
            || self.fullscreen_controls_visible
            || self.open_track_select.is_some();
        self.fullscreen_cursor_visible = false;
        self.fullscreen_controls_visible = false;
        self.open_track_select = None;
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
        if !window.is_fullscreen() {
            return;
        }

        let bounds = window_viewport_bounds(window);
        let in_controls = fullscreen_progress_controls_contains(event.position, bounds);
        let in_hot_zone = fullscreen_controls_hot_zone_contains(event.position, bounds);

        let controls_visible = self.fullscreen_controls_visible || in_controls || in_hot_zone;
        let changed = !self.fullscreen_cursor_visible
            || self.fullscreen_controls_visible != controls_visible
            || self.fullscreen_mouse_in_controls != in_controls;

        self.fullscreen_cursor_visible = true;
        self.fullscreen_controls_visible = controls_visible;
        self.fullscreen_mouse_in_controls = in_controls;
        self.schedule_fullscreen_controls_hide(cx);

        if changed {
            cx.notify();
        }
    }
}

pub(super) fn window_viewport_bounds(window: &Window) -> Bounds<Pixels> {
    Bounds::new(gpui::point(px(0.0), px(0.0)), window.viewport_size())
}

pub(super) fn playback_progress_bar_visible(
    is_fullscreen: bool,
    fullscreen_controls_visible: bool,
) -> bool {
    !is_fullscreen || fullscreen_controls_visible
}

pub(super) fn fullscreen_controls_hot_zone_contains(
    position: Point<Pixels>,
    viewport_bounds: Bounds<Pixels>,
) -> bool {
    position.y
        >= viewport_bounds.origin.y
            + viewport_bounds.size.height * FULLSCREEN_CONTROLS_HOT_ZONE_FRACTION
}

pub(super) fn fullscreen_progress_controls_contains(
    position: Point<Pixels>,
    viewport_bounds: Bounds<Pixels>,
) -> bool {
    playback_progress_bar_bounds(viewport_bounds).contains(&position)
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
