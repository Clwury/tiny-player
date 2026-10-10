//! Page-owned native input and main-thread action delivery.
use super::*;
use crate::player::gamepad::GamepadInput;
use crate::player::model::shortcuts::PlaybackShortcut;

const GAMEPAD_CONTROLS_HIDE_DELAY: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(super) struct PlaybackGamepad {
    started: bool,
    input: Option<GamepadInput>,
    delivery: Option<gpui::Task<()>>,
    activation: Option<gpui::Subscription>,
    closed: Option<gpui::Subscription>,
}

impl PlaybackGamepad {
    pub(super) fn stop(&mut self) {
        self.started = true;
        self.input = None;
        self.delivery = None;
        self.activation = None;
        self.closed = None;
    }

    #[cfg(test)]
    pub(super) fn disabled() -> Self {
        Self {
            started: true,
            ..Self::default()
        }
    }
}

impl PlaybackPage {
    pub(super) fn start_gamepad_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.gamepad.started {
            return;
        }
        self.gamepad.started = true;
        let (input, receiver) = match GamepadInput::start() {
            Ok(input) => input,
            Err(error) => {
                tracing::warn!(%error, "Could not start playback gamepad input");
                return;
            }
        };
        self.attach_gamepad_input(input, receiver, window, cx);
    }

    fn attach_gamepad_input(
        &mut self,
        input: GamepadInput,
        receiver: async_channel::Receiver<crate::player::gamepad::GamepadMessage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        input.set_active(window.is_window_active());
        self.gamepad.input = Some(input);
        self.gamepad.activation = Some(cx.observe_window_activation(window, |page, window, _| {
            if let Some(input) = &page.gamepad.input {
                input.set_active(window.is_window_active());
            }
        }));
        let window_id = window.window_handle().window_id();
        let page = cx.weak_entity();
        self.gamepad.closed = Some(cx.on_window_closed(move |cx, closed_window| {
            if closed_window == window_id {
                page.update(cx, |page, _| page.gamepad.stop()).ok();
            }
        }));
        self.gamepad.delivery = Some(cx.spawn_in(window, async move |page, cx| {
            while let Ok(message) = receiver.recv().await {
                if page
                    .update_in(cx, |page, window, cx| {
                        if window.is_window_active()
                            && page
                                .gamepad
                                .input
                                .as_ref()
                                .is_some_and(|input| input.accepts(&message))
                        {
                            page.handle_gamepad_shortcut(message.shortcut, window, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    pub(super) fn handle_gamepad_shortcut(
        &mut self,
        shortcut: PlaybackShortcut,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Menus retain their existing mouse interaction. B dismisses them;
        // directional shortcuts must not seek while a menu is being inspected.
        let menu_open = self.presentation.episode_list.open
            || self.presentation.track_select_open.is_some()
            || self.presentation.timeline_presentation.cache_status_open;
        if shortcut != PlaybackShortcut::Back && menu_open {
            return;
        }
        let returning_to_detail = shortcut == PlaybackShortcut::Back
            && !menu_open
            && !self.presentation.playback_details_visible;
        if !self.dispatch_playback_shortcut(shortcut, window, cx) || returning_to_detail {
            return;
        }
        if shortcut != PlaybackShortcut::ToggleControls {
            self.presentation.fullscreen.controls_visible = true;
        }
        if self.presentation.fullscreen.controls_visible {
            self.schedule_presentation_timer(
                PresentationTimer::Controls,
                GAMEPAD_CONTROLS_HIDE_DELAY,
                cx,
            );
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests;
