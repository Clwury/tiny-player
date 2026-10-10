use super::*;

fn enabled_controls() -> GamepadControls {
    let mut controls = GamepadControls::default();
    controls.set_enabled(true);
    controls
}

#[test]
fn gamepad_directions_adjust_volume_and_seek_at_distinct_intervals() {
    let mut controls = enabled_controls();
    let now = Instant::now();
    for (button, shortcut) in [
        (GamepadButton::Left, PlaybackShortcut::SeekRelative(-5)),
        (GamepadButton::Right, PlaybackShortcut::SeekRelative(5)),
        (
            GamepadButton::LeftShoulder,
            PlaybackShortcut::SeekRelative(-60),
        ),
        (
            GamepadButton::RightShoulder,
            PlaybackShortcut::SeekRelative(60),
        ),
        (GamepadButton::Up, PlaybackShortcut::IncreaseVolume),
        (GamepadButton::Down, PlaybackShortcut::DecreaseVolume),
    ] {
        assert_eq!(controls.button_value(0, button, 1.0, now), Some(shortcut));
        controls.button_value(0, button, 0.0, now);
    }
}

#[test]
fn held_toggle_buttons_execute_once_until_released() {
    let mut controls = enabled_controls();
    let now = Instant::now();
    for button in [
        GamepadButton::South,
        GamepadButton::East,
        GamepadButton::West,
        GamepadButton::North,
        GamepadButton::Start,
        GamepadButton::Select,
        GamepadButton::RightThumb,
    ] {
        let shortcut = controls.button_value(0, button, 1.0, now).unwrap();
        assert_eq!(controls.button_value(0, button, 1.0, now), None);
        assert!(controls.repeats(now + Duration::from_secs(10)).is_empty());
        controls.button_value(0, button, 0.0, now);
        assert_eq!(controls.button_value(0, button, 1.0, now), Some(shortcut));
        controls.button_value(0, button, 0.0, now);
    }
}

#[test]
fn seek_repeat_waits_for_delay_and_never_catches_up_after_a_stall() {
    let mut controls = enabled_controls();
    let now = Instant::now();
    let seek = PlaybackShortcut::SeekRelative(5);
    assert_eq!(
        controls.button_value(0, GamepadButton::Right, 1.0, now),
        Some(seek)
    );
    assert!(
        controls
            .repeats(now + Duration::from_millis(399))
            .is_empty()
    );
    assert_eq!(controls.repeats(now + REPEAT_DELAY), vec![seek]);
    assert!(
        controls
            .repeats(now + Duration::from_millis(649))
            .is_empty()
    );
    assert_eq!(
        controls.repeats(now + Duration::from_millis(650)),
        vec![seek]
    );
    assert_eq!(controls.repeats(now + Duration::from_secs(30)), vec![seek]);
    assert!(controls.repeats(now + Duration::from_secs(30)).is_empty());
    controls.button_value(0, GamepadButton::Right, 0.0, now);
    assert!(controls.repeats(now + Duration::from_secs(60)).is_empty());
}

#[test]
fn focus_loss_stops_repeat_and_reactivation_requires_a_new_press() {
    let mut controls = enabled_controls();
    let now = Instant::now();
    controls.button_value(0, GamepadButton::Up, 1.0, now);
    controls.set_enabled(false);
    assert!(controls.repeats(now + REPEAT_DELAY).is_empty());
    assert_eq!(
        controls.button_value(0, GamepadButton::South, 1.0, now),
        None
    );
    controls.set_enabled(true);
    assert!(controls.repeats(now + Duration::from_secs(10)).is_empty());
    assert_eq!(
        controls.button_value(0, GamepadButton::South, 1.0, now),
        None
    );
    controls.button_value(0, GamepadButton::South, 0.0, now);
    assert_eq!(
        controls.button_value(0, GamepadButton::South, 1.0, now),
        Some(PlaybackShortcut::TogglePlayback)
    );
}

#[test]
fn one_device_owns_input_and_disconnect_releases_it() {
    let mut controls = enabled_controls();
    let now = Instant::now();
    assert_eq!(
        controls.button_value(0, GamepadButton::Right, 1.0, now),
        Some(PlaybackShortcut::SeekRelative(5))
    );
    assert_eq!(
        controls.button_value(1, GamepadButton::South, 1.0, now),
        None
    );
    controls.disconnect(0);
    assert!(controls.repeats(now + Duration::from_secs(10)).is_empty());
    assert_eq!(
        controls.button_value(1, GamepadButton::South, 1.0, now),
        None
    );
    controls.button_value(1, GamepadButton::South, 0.0, now);
    assert_eq!(
        controls.button_value(1, GamepadButton::South, 1.0, now),
        Some(PlaybackShortcut::TogglePlayback)
    );
}

#[test]
fn analog_trigger_hysteresis_ignores_jitter_and_does_not_repeat_rate_changes() {
    let mut controls = enabled_controls();
    let now = Instant::now();
    let button = GamepadButton::RightTrigger;
    let increase = PlaybackShortcut::ChangeRate(PlaybackRateChange::Increase);
    assert_eq!(controls.button_value(0, button, 0.54, now), None);
    assert_eq!(controls.button_value(0, button, 0.6, now), Some(increase));
    for value in [0.9, 0.5, 0.36, 0.6, f32::NAN, f32::INFINITY] {
        assert_eq!(controls.button_value(0, button, value, now), None);
    }
    assert!(controls.repeats(now + Duration::from_secs(10)).is_empty());
    controls.button_value(0, button, 0.34, now);
    assert_eq!(controls.button_value(0, button, 0.6, now), Some(increase));
}

#[test]
fn left_stick_click_modifies_subtitle_position_without_adjusting_volume() {
    let mut controls = enabled_controls();
    let now = Instant::now();
    assert_eq!(
        controls.button_value(0, GamepadButton::LeftThumb, 1.0, now),
        None
    );
    assert_eq!(
        controls.button_value(0, GamepadButton::Up, 1.0, now),
        Some(PlaybackShortcut::RaiseSubtitle)
    );
    assert_eq!(
        controls.button_value(0, GamepadButton::Down, 1.0, now),
        Some(PlaybackShortcut::LowerSubtitle)
    );
    assert!(controls.repeats(now + Duration::from_secs(10)).is_empty());
    controls.button_value(0, GamepadButton::LeftThumb, 0.0, now);
    assert!(controls.repeats(now + Duration::from_secs(20)).is_empty());
}

#[test]
fn pressing_subtitle_modifier_cancels_existing_volume_repeat() {
    let mut controls = enabled_controls();
    let now = Instant::now();
    controls.button_value(0, GamepadButton::Up, 1.0, now);
    controls.button_value(0, GamepadButton::LeftThumb, 1.0, now);
    assert!(controls.repeats(now + REPEAT_DELAY).is_empty());
    controls.button_value(0, GamepadButton::LeftThumb, 0.0, now);
    assert!(controls.repeats(now + Duration::from_secs(10)).is_empty());
}
