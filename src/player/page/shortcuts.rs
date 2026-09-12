use super::subtitles::subtitle_vertical_adjust_step;
use super::*;
use crate::player::rate::PlaybackRateChange;

const KEYBOARD_SEEK_STEP_SECONDS: i32 = 5;
const KEYBOARD_LONG_SEEK_STEP_SECONDS: i32 = 60;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PlaybackShortcut {
    TogglePlayback,
    ToggleFullscreen,
    ExitFullscreen,
    SeekRelative(i32),
    ToggleInfoOverlay,
    RaiseSubtitle,
    LowerSubtitle,
    DecreaseVolume,
    IncreaseVolume,
    ToggleMute,
    ChangeRate(PlaybackRateChange),
}

impl PlaybackPage {
    pub(super) fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(shortcut) = playback_shortcut_for_event(event) else {
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
            PlaybackShortcut::SeekRelative(seconds) => {
                self.seek_relative(f64::from(seconds), window, cx);
            }
            PlaybackShortcut::ToggleInfoOverlay => {
                self.playback_details_visible = !self.playback_details_visible;
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
            PlaybackShortcut::DecreaseVolume => {
                self.adjust_playback_volume(-PLAYBACK_VOLUME_STEP, cx);
            }
            PlaybackShortcut::IncreaseVolume => {
                self.adjust_playback_volume(PLAYBACK_VOLUME_STEP, cx);
            }
            PlaybackShortcut::ToggleMute => self.toggle_playback_mute(cx),
            PlaybackShortcut::ChangeRate(change) => self.change_playback_rate(change, cx),
        }
    }
}

fn playback_shortcut_for_event(event: &KeyDownEvent) -> Option<PlaybackShortcut> {
    let keystroke = &event.keystroke;
    let modifiers = keystroke.modifiers;
    if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
        return None;
    }

    let key = if modifiers.shift {
        // Platforms may report either the typed symbol or the unshifted key.
        keystroke
            .key_char
            .as_deref()
            .or(match keystroke.key.as_str() {
                "[" => Some("{"),
                "]" => Some("}"),
                "/" | "*" | "{" | "}" => Some(keystroke.key.as_str()),
                _ => None,
            })?
    } else {
        &keystroke.key
    };
    let shortcut = playback_shortcut_for_key(key)?;
    let adjusts_volume = matches!(
        shortcut,
        PlaybackShortcut::DecreaseVolume | PlaybackShortcut::IncreaseVolume
    );
    let adjusts_rate = matches!(shortcut, PlaybackShortcut::ChangeRate(_));
    if modifiers.shift && !adjusts_volume && !(adjusts_rate && matches!(key, "[" | "]" | "{" | "}"))
    {
        return None;
    }
    if event.is_held
        && !adjusts_volume
        && !matches!(
            shortcut,
            PlaybackShortcut::ChangeRate(
                PlaybackRateChange::Decrease
                    | PlaybackRateChange::Increase
                    | PlaybackRateChange::Halve
                    | PlaybackRateChange::Double
            )
        )
    {
        return None;
    }
    Some(shortcut)
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
        Some(PlaybackShortcut::SeekRelative(-KEYBOARD_SEEK_STEP_SECONDS))
    } else if key.eq_ignore_ascii_case("right") {
        Some(PlaybackShortcut::SeekRelative(KEYBOARD_SEEK_STEP_SECONDS))
    } else if key.eq_ignore_ascii_case("up") {
        Some(PlaybackShortcut::SeekRelative(
            KEYBOARD_LONG_SEEK_STEP_SECONDS,
        ))
    } else if key.eq_ignore_ascii_case("down") {
        Some(PlaybackShortcut::SeekRelative(
            -KEYBOARD_LONG_SEEK_STEP_SECONDS,
        ))
    } else if key.eq_ignore_ascii_case("i") {
        Some(PlaybackShortcut::ToggleInfoOverlay)
    } else if key.eq_ignore_ascii_case("r") {
        Some(PlaybackShortcut::RaiseSubtitle)
    } else if key.eq_ignore_ascii_case("t") {
        Some(PlaybackShortcut::LowerSubtitle)
    } else if matches!(key, "9" | "/")
        || key.eq_ignore_ascii_case("divide")
        || key.eq_ignore_ascii_case("kp_divide")
    {
        Some(PlaybackShortcut::DecreaseVolume)
    } else if matches!(key, "0" | "*")
        || key.eq_ignore_ascii_case("multiply")
        || key.eq_ignore_ascii_case("kp_multiply")
    {
        Some(PlaybackShortcut::IncreaseVolume)
    } else if key == "[" {
        Some(PlaybackShortcut::ChangeRate(PlaybackRateChange::Decrease))
    } else if key == "]" {
        Some(PlaybackShortcut::ChangeRate(PlaybackRateChange::Increase))
    } else if key == "{" {
        Some(PlaybackShortcut::ChangeRate(PlaybackRateChange::Halve))
    } else if key == "}" {
        Some(PlaybackShortcut::ChangeRate(PlaybackRateChange::Double))
    } else if key.eq_ignore_ascii_case("backspace") {
        Some(PlaybackShortcut::ChangeRate(PlaybackRateChange::Reset))
    } else if key.eq_ignore_ascii_case("m") {
        Some(PlaybackShortcut::ToggleMute)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shortcut_for_keystroke(key: &str, is_held: bool) -> Option<PlaybackShortcut> {
        playback_shortcut_for_event(&KeyDownEvent {
            keystroke: gpui::Keystroke::parse(key).unwrap(),
            is_held,
            prefer_character_input: false,
        })
    }

    #[test]
    fn mpv_volume_shortcuts_support_the_number_row_and_keypad() {
        for key in ["9", "/", "divide", "KP_DIVIDE"] {
            assert_eq!(
                shortcut_for_keystroke(key, false),
                Some(PlaybackShortcut::DecreaseVolume),
                "{key}"
            );
        }
        for key in ["0", "*", "multiply", "KP_MULTIPLY"] {
            assert_eq!(
                shortcut_for_keystroke(key, false),
                Some(PlaybackShortcut::IncreaseVolume),
                "{key}"
            );
        }
        assert_eq!(
            shortcut_for_keystroke("m", false),
            Some(PlaybackShortcut::ToggleMute)
        );
    }

    #[test]
    fn mpv_speed_shortcuts_handle_symbols_shift_and_repeat() {
        use PlaybackRateChange::*;
        for (key, change) in [
            ("[", Decrease),
            ("]", Increase),
            ("{", Halve),
            ("}", Double),
            ("shift-[->{", Halve),
            ("shift-]->}", Double),
            ("shift-[", Halve),
            ("shift-]", Double),
            ("shift-{", Halve),
            ("shift-}", Double),
        ] {
            for held in [false, true] {
                assert_eq!(
                    shortcut_for_keystroke(key, held),
                    Some(PlaybackShortcut::ChangeRate(change)),
                    "{key}"
                );
            }
        }
        assert_eq!(
            shortcut_for_keystroke("backspace", false),
            Some(PlaybackShortcut::ChangeRate(Reset))
        );
        assert_eq!(shortcut_for_keystroke("backspace", true), None);
        assert_eq!(shortcut_for_keystroke("shift-backspace", false), None);
        assert_eq!(
            shortcut_for_keystroke("shift-backspace->backspace", false),
            None
        );
        for modifier in ["ctrl", "alt", "super", "fn"] {
            for key in ["[", "]", "{", "}", "backspace"] {
                assert_eq!(
                    shortcut_for_keystroke(&format!("{modifier}-{key}"), false),
                    None
                );
            }
        }
    }

    #[test]
    fn shifted_volume_shortcuts_match_the_typed_character() {
        for key in ["shift-8->*", "shift-*", "shift-0->0"] {
            assert_eq!(
                shortcut_for_keystroke(key, false),
                Some(PlaybackShortcut::IncreaseVolume),
                "{key}"
            );
        }
        for key in ["shift-9->9", "shift-7->/"] {
            assert_eq!(
                shortcut_for_keystroke(key, false),
                Some(PlaybackShortcut::DecreaseVolume),
                "{key}"
            );
        }
        for key in [
            "shift-9->(",
            "shift-0->)",
            "shift-/->?",
            "shift-m->M",
            "shift-9",
        ] {
            assert_eq!(shortcut_for_keystroke(key, false), None, "{key}");
        }
    }

    #[test]
    fn playback_shortcuts_ignore_control_alt_platform_and_function_modifiers() {
        for modifier in ["ctrl", "alt", "super", "fn"] {
            for key in [
                "9",
                "0",
                "/",
                "*",
                "m",
                "shift-8->*",
                "left",
                "right",
                "up",
                "down",
            ] {
                let keystroke = format!("{modifier}-{key}");
                assert_eq!(
                    shortcut_for_keystroke(&keystroke, false),
                    None,
                    "{keystroke}"
                );
            }
        }
    }

    #[test]
    fn held_keys_repeat_volume_adjustment_but_not_toggles_or_seeks() {
        for key in ["9", "0", "/", "*", "divide", "multiply", "shift-8->*"] {
            assert!(shortcut_for_keystroke(key, true).is_some(), "{key}");
        }
        for key in [
            "m", "space", "p", "f", "escape", "i", "r", "t", "left", "right", "up", "down",
        ] {
            assert_eq!(shortcut_for_keystroke(key, true), None, "{key}");
        }
    }

    #[test]
    fn arrow_shortcuts_seek_with_mpv_intervals() {
        for (key, seconds) in [("left", -5), ("right", 5), ("up", 60), ("down", -60)] {
            assert_eq!(
                shortcut_for_keystroke(key, false),
                Some(PlaybackShortcut::SeekRelative(seconds)),
                "{key}"
            );
            assert_eq!(
                playback_shortcut_for_key(&key.to_uppercase()),
                Some(PlaybackShortcut::SeekRelative(seconds))
            );
            assert_eq!(shortcut_for_keystroke(&format!("shift-{key}"), false), None);
        }
    }

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
            Some(PlaybackShortcut::SeekRelative(-5))
        );
        assert_eq!(
            playback_shortcut_for_key("right"),
            Some(PlaybackShortcut::SeekRelative(5))
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
