//! Video surface input, drag capture and volume gestures.
use super::*;

impl PlaybackPage {
    pub(super) fn handle_surface_left_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.close_track_select(cx) {
            cx.stop_propagation();
            return;
        }
        self.presentation.window_drag = if event.click_count == 1
            && !window.is_fullscreen()
            && !window_uses_system_decorations(window)
        {
            WindowDragState::Pending
        } else {
            WindowDragState::Idle
        };
        // GPUI's Windows backend starts native moves from an unhandled
        // non-client press. Its start_window_move() implementation is a no-op.
        // Keep double clicks and menu dismissal in the player instead of
        // letting Windows maximize the window or start a move.
        if !cfg!(target_os = "windows") || self.presentation.window_drag != WindowDragState::Pending
        {
            cx.stop_propagation();
        }
        if event.click_count == 2 {
            self.toggle_playback_fullscreen(window, cx);
        }
    }

    pub(super) fn handle_surface_right_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if self.close_track_select(cx) {
            return;
        }
        if event.click_count == 1 {
            self.toggle_playback_pause_command(cx);
        }
    }

    pub(super) fn handle_surface_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let delta = volume_delta_from_scroll_delta(event.delta);
        if delta.abs() < f32::EPSILON {
            return;
        }
        self.adjust_playback_volume(delta, cx);
    }

    pub(super) fn handle_surface_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !event.dragging()
            || (window_uses_system_decorations(window)
                && self.presentation.window_drag == WindowDragState::Pending)
        {
            self.presentation.window_drag = WindowDragState::Idle;
        }
        if !cfg!(target_os = "windows")
            && self.presentation.window_drag == WindowDragState::Pending
            && !window.is_fullscreen()
            && event.dragging()
            && self.session.timeline().progress_drag_position.is_none()
        {
            // The compositor may consume the release after taking the pointer.
            self.presentation.window_drag = WindowDragState::Idle;
            cx.stop_propagation();
            window.start_window_move();
            return;
        }

        self.handle_mouse_move(event, window, cx);
    }

    pub(super) fn render_mouse_capture(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let record_press = cx.listener(|page, in_playback: &bool, _, _| {
            page.presentation.window_drag = if *in_playback {
                WindowDragState::Blocked
            } else {
                WindowDragState::Idle
            };
        });
        let reset_on_release = cx.listener(|page, event: &MouseUpEvent, _, _| {
            if event.button == MouseButton::Left {
                page.presentation.window_drag = WindowDragState::Idle;
            }
        });
        let stop_control_drag = cx.listener(|page, _: &MouseMoveEvent, _, cx| {
            if page.presentation.window_drag == WindowDragState::Blocked {
                cx.stop_propagation();
            }
        });
        let drag_observer = canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                // Observe presses and releases before controls handle or occlude
                // them. Only a subsequent surface press may arm a window move.
                window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                    if phase.capture() {
                        record_press(
                            &(event.button == MouseButton::Left
                                && bounds.contains(&event.position)),
                            window,
                            cx,
                        );
                    }
                });
                window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                    if phase.capture() {
                        reset_on_release(event, window, cx);
                    }
                });
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                    // A control press must not turn into the titlebar's own
                    // window drag either. Child controls handle drags first.
                    if phase.bubble() && event.dragging() && !bounds.contains(&event.position) {
                        stop_control_drag(event, window, cx);
                    }
                });
            },
        )
        .absolute()
        .size_full();

        div()
            .id("playback-mouse-capture")
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .left_0()
            .when(
                cfg!(target_os = "windows") && !window.is_fullscreen(),
                |this| {
                    // Use the same native hit testing as the titlebar. Controls
                    // above this surface occlude it and keep their own gestures.
                    this.window_control_area(gpui::WindowControlArea::Drag)
                        // WM_NCRBUTTONUP would otherwise open the system menu
                        // after our right-button press toggles playback pause.
                        .on_mouse_up(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                },
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(Self::handle_surface_left_mouse_down),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(Self::handle_surface_right_mouse_down),
            )
            .on_mouse_move(cx.listener(Self::handle_surface_mouse_move))
            .on_scroll_wheel(cx.listener(Self::handle_surface_scroll_wheel))
            .child(drag_observer)
    }
}

pub(super) fn playback_volume_percent(volume: f32) -> u32 {
    (clamp_playback_volume(volume) * 100.0).round() as u32
}

pub(super) const PLAYBACK_VOLUME_STEP: f32 = 0.02;
// GPUI's Linux Wayland and X11 backends report three lines per wheel detent.
const SCROLL_LINES_PER_VOLUME_STEP: f32 = 3.0;

pub(super) fn volume_delta_from_scroll_delta(delta: ScrollDelta) -> f32 {
    match delta {
        ScrollDelta::Lines(point) => point.y / SCROLL_LINES_PER_VOLUME_STEP * PLAYBACK_VOLUME_STEP,
        ScrollDelta::Pixels(point) => f32::from(point.y) / 25.0 * PLAYBACK_VOLUME_STEP,
    }
    .clamp(-0.2, 0.2)
}
