//! Button edges and repeat policy; the native adapter supplies devices and time.
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use super::shortcuts::{LONG_SEEK_STEP_SECONDS, PlaybackShortcut, SEEK_STEP_SECONDS};
use tiny_playback::PlaybackRateChange;

const REPEAT_DELAY: Duration = Duration::from_millis(400);
const PRESS_THRESHOLD: f32 = 0.55;
const RELEASE_THRESHOLD: f32 = 0.35;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(in crate::player) enum GamepadButton {
    South,
    East,
    West,
    North,
    Left,
    Right,
    Up,
    Down,
    LeftShoulder,
    RightShoulder,
    Start,
    Select,
    LeftTrigger,
    RightTrigger,
    LeftThumb,
    RightThumb,
}

#[derive(Default)]
struct DeviceButtons {
    pressed: HashSet<GamepadButton>,
    repeats: HashMap<GamepadButton, Instant>,
}

#[derive(Default)]
pub(in crate::player) struct GamepadControls {
    enabled: bool,
    active_device: Option<usize>,
    devices: HashMap<usize, DeviceButtons>,
}

impl GamepadControls {
    pub(in crate::player) fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.enabled = enabled;
            for device in self.devices.values_mut() {
                device.repeats.clear();
            }
        }
        // Keep physical button state: reactivation must wait for a fresh press.
    }

    pub(in crate::player) fn button_value(
        &mut self,
        device_id: usize,
        button: GamepadButton,
        value: f32,
        now: Instant,
    ) -> Option<PlaybackShortcut> {
        if !value.is_finite() {
            return None;
        }
        let device = self.devices.entry(device_id).or_default();
        if value <= RELEASE_THRESHOLD {
            device.pressed.remove(&button);
            device.repeats.remove(&button);
            return None;
        }
        if value < PRESS_THRESHOLD || !device.pressed.insert(button) {
            return None;
        }
        if !self.enabled {
            return None;
        }
        let active_device = self.active_device.get_or_insert(device_id);
        if *active_device != device_id {
            return None;
        }
        let shortcut =
            shortcut_for_button(button, device.pressed.contains(&GamepadButton::LeftThumb))?;
        if shortcut.gamepad_repeat_interval().is_some() {
            device.repeats.insert(button, now + REPEAT_DELAY);
        }
        Some(shortcut)
    }

    pub(in crate::player) fn disconnect(&mut self, device_id: usize) {
        self.devices.remove(&device_id);
        if self.active_device == Some(device_id) {
            self.active_device = None;
        }
    }

    pub(in crate::player) fn repeats(&mut self, now: Instant) -> Vec<PlaybackShortcut> {
        if !self.enabled {
            return Vec::new();
        }
        let Some(device) = self.active_device.and_then(|id| self.devices.get_mut(&id)) else {
            return Vec::new();
        };
        let subtitle_modifier = device.pressed.contains(&GamepadButton::LeftThumb);
        let mut shortcuts = Vec::new();
        device.repeats.retain(|button, deadline| {
            let Some(shortcut) = shortcut_for_button(*button, subtitle_modifier) else {
                return false;
            };
            let Some(interval) = shortcut.gamepad_repeat_interval() else {
                return false;
            };
            if now >= *deadline {
                // A delayed tick produces one command, never a catch-up burst.
                *deadline = now + interval;
                shortcuts.push(shortcut);
            }
            true
        });
        shortcuts
    }
}

fn shortcut_for_button(button: GamepadButton, subtitle_modifier: bool) -> Option<PlaybackShortcut> {
    use GamepadButton::*;
    Some(match button {
        South => PlaybackShortcut::TogglePlayback,
        East => PlaybackShortcut::Back,
        West => PlaybackShortcut::ToggleMute,
        North => PlaybackShortcut::ToggleInfoOverlay,
        Left => PlaybackShortcut::SeekRelative(-SEEK_STEP_SECONDS),
        Right => PlaybackShortcut::SeekRelative(SEEK_STEP_SECONDS),
        Up if subtitle_modifier => PlaybackShortcut::RaiseSubtitle,
        Down if subtitle_modifier => PlaybackShortcut::LowerSubtitle,
        Up => PlaybackShortcut::IncreaseVolume,
        Down => PlaybackShortcut::DecreaseVolume,
        LeftShoulder => PlaybackShortcut::SeekRelative(-LONG_SEEK_STEP_SECONDS),
        RightShoulder => PlaybackShortcut::SeekRelative(LONG_SEEK_STEP_SECONDS),
        Start => PlaybackShortcut::ToggleFullscreen,
        Select => PlaybackShortcut::ToggleControls,
        LeftTrigger => PlaybackShortcut::ChangeRate(PlaybackRateChange::Decrease),
        RightTrigger => PlaybackShortcut::ChangeRate(PlaybackRateChange::Increase),
        RightThumb => PlaybackShortcut::ChangeRate(PlaybackRateChange::Reset),
        LeftThumb => return None,
    })
}

#[cfg(test)]
mod tests;
