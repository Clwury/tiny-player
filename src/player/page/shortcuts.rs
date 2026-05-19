use super::subtitles::subtitle_vertical_adjust_step;
use super::*;

const KEYBOARD_SEEK_STEP_SECONDS: f64 = 5.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PlaybackShortcut {
    TogglePlayback,
    ToggleFullscreen,
    ExitFullscreen,
    SeekBackward,
    SeekForward,
    ToggleInfoOverlay,
    RaiseSubtitle,
    LowerSubtitle,
}

impl PlaybackPage {
    pub(super) fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.is_held || event.keystroke.modifiers.modified() {
            return;
        }

        let Some(shortcut) = playback_shortcut_for_key(&event.keystroke.key) else {
            return;
        };
        if shortcut == PlaybackShortcut::ExitFullscreen && !window.is_fullscreen() {
            return;
        }

        cx.stop_propagation();
        match shortcut {
            PlaybackShortcut::TogglePlayback => self.toggle_playback_pause_command(cx),
            PlaybackShortcut::ToggleFullscreen => {
                self.reset_fullscreen_controls();
                window.toggle_fullscreen();
                cx.notify();
            }
            PlaybackShortcut::ExitFullscreen => {
                self.reset_fullscreen_controls();
                window.toggle_fullscreen();
                cx.notify();
            }
            PlaybackShortcut::SeekBackward => {
                self.seek_relative(-KEYBOARD_SEEK_STEP_SECONDS, window, cx);
            }
            PlaybackShortcut::SeekForward => {
                self.seek_relative(KEYBOARD_SEEK_STEP_SECONDS, window, cx);
            }
            PlaybackShortcut::ToggleInfoOverlay => {
                self.playback_info_overlay_visible = !self.playback_info_overlay_visible;
                cx.notify();
            }
            PlaybackShortcut::RaiseSubtitle => {
                self.adjust_subtitle_vertical_offset_fraction(
                    subtitle_vertical_adjust_step(),
                    window,
                    cx,
                );
            }
            PlaybackShortcut::LowerSubtitle => {
                self.adjust_subtitle_vertical_offset_fraction(
                    -subtitle_vertical_adjust_step(),
                    window,
                    cx,
                );
            }
        }
    }
}

pub(super) fn playback_shortcut_for_key(key: &str) -> Option<PlaybackShortcut> {
    if key == " " || key.eq_ignore_ascii_case("space") {
        return Some(PlaybackShortcut::TogglePlayback);
    }

    if key.eq_ignore_ascii_case("p") {
        Some(PlaybackShortcut::TogglePlayback)
    } else if key.eq_ignore_ascii_case("f") {
        Some(PlaybackShortcut::ToggleFullscreen)
    } else if key.eq_ignore_ascii_case("escape") {
        Some(PlaybackShortcut::ExitFullscreen)
    } else if key.eq_ignore_ascii_case("left") {
        Some(PlaybackShortcut::SeekBackward)
    } else if key.eq_ignore_ascii_case("right") {
        Some(PlaybackShortcut::SeekForward)
    } else if key.eq_ignore_ascii_case("i") {
        Some(PlaybackShortcut::ToggleInfoOverlay)
    } else if key.eq_ignore_ascii_case("r") {
        Some(PlaybackShortcut::RaiseSubtitle)
    } else if key.eq_ignore_ascii_case("t") {
        Some(PlaybackShortcut::LowerSubtitle)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_shortcut_keys_map_to_player_actions() {
        assert_eq!(
            playback_shortcut_for_key("space"),
            Some(PlaybackShortcut::TogglePlayback)
        );
        assert_eq!(
            playback_shortcut_for_key(" "),
            Some(PlaybackShortcut::TogglePlayback)
        );
        assert_eq!(
            playback_shortcut_for_key("p"),
            Some(PlaybackShortcut::TogglePlayback)
        );
        assert_eq!(
            playback_shortcut_for_key("f"),
            Some(PlaybackShortcut::ToggleFullscreen)
        );
        assert_eq!(
            playback_shortcut_for_key("escape"),
            Some(PlaybackShortcut::ExitFullscreen)
        );
        assert_eq!(
            playback_shortcut_for_key("left"),
            Some(PlaybackShortcut::SeekBackward)
        );
        assert_eq!(
            playback_shortcut_for_key("right"),
            Some(PlaybackShortcut::SeekForward)
        );
        assert_eq!(
            playback_shortcut_for_key("i"),
            Some(PlaybackShortcut::ToggleInfoOverlay)
        );
        assert_eq!(
            playback_shortcut_for_key("r"),
            Some(PlaybackShortcut::RaiseSubtitle)
        );
        assert_eq!(
            playback_shortcut_for_key("t"),
            Some(PlaybackShortcut::LowerSubtitle)
        );
        assert_eq!(playback_shortcut_for_key("enter"), None);
    }
}
