//! gilrs ownership stays off the UI thread; delivery is bounded and focus-fenced.
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use async_channel::{Receiver, Sender};
use gilrs::{Button, Event, EventType, Gilrs};

use super::model::{
    gamepad::{GamepadButton, GamepadControls},
    shortcuts::PlaybackShortcut,
};

const INPUT_WAIT: Duration = Duration::from_millis(16);
const MAX_DELIVERY_AGE: Duration = Duration::from_millis(250);

pub(super) struct GamepadMessage {
    pub(super) shortcut: PlaybackShortcut,
    epoch: u64,
    created: Instant,
}

#[derive(Default)]
struct InputGate {
    epoch: AtomicU64,
    stopped: AtomicBool,
}

impl InputGate {
    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    fn set_active(&self, active: bool) {
        let _ = self
            .epoch
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
                ((epoch & 1 != 0) != active)
                    .then_some((epoch.wrapping_add(2) & !1) | u64::from(active))
            });
    }

    fn invalidate(&self) {
        self.epoch.fetch_add(2, Ordering::AcqRel);
    }

    fn accepts(&self, epoch: u64) -> bool {
        epoch & 1 != 0 && self.epoch() == epoch && !self.stopped.load(Ordering::Acquire)
    }
}

pub(super) struct GamepadInput {
    gate: Arc<InputGate>,
    wake: Option<std::thread::Thread>,
}

impl GamepadInput {
    pub(super) fn start() -> std::io::Result<(Self, Receiver<GamepadMessage>)> {
        let gate = Arc::new(InputGate::default());
        let (sender, receiver) = async_channel::bounded(32);
        let worker_gate = gate.clone();
        let worker = std::thread::Builder::new()
            .name("playback-gamepad".into())
            .spawn(move || run(worker_gate, sender))?;
        Ok((
            Self {
                gate,
                wake: Some(worker.thread().clone()),
            },
            receiver,
        ))
    }

    pub(super) fn set_active(&self, active: bool) {
        self.gate.set_active(active);
        if let Some(worker) = &self.wake {
            worker.unpark();
        }
    }

    pub(super) fn accepts(&self, message: &GamepadMessage) -> bool {
        self.gate.accepts(message.epoch) && message.created.elapsed() <= MAX_DELIVERY_AGE
    }

    #[cfg(test)]
    pub(super) fn test_channel() -> (Self, Sender<GamepadMessage>, Receiver<GamepadMessage>) {
        let (sender, receiver) = async_channel::bounded(32);
        (
            Self {
                gate: Arc::new(InputGate::default()),
                wake: None,
            },
            sender,
            receiver,
        )
    }

    #[cfg(test)]
    pub(super) fn message_for_test(&self, shortcut: PlaybackShortcut) -> GamepadMessage {
        GamepadMessage {
            shortcut,
            epoch: self.gate.epoch(),
            created: Instant::now(),
        }
    }
}

impl Drop for GamepadInput {
    fn drop(&mut self) {
        self.gate.stopped.store(true, Ordering::Release);
        self.gate.invalidate();
        if let Some(worker) = &self.wake {
            worker.unpark();
        }
    }
}

fn run(gate: Arc<InputGate>, sender: Sender<GamepadMessage>) {
    let mut gilrs = match Gilrs::new() {
        Ok(gilrs) => gilrs,
        Err(error) => {
            tracing::warn!(%error, "Gamepad input unavailable");
            return;
        }
    };
    let mut controls = GamepadControls::default();
    let mut epoch = u64::MAX;
    while !gate.stopped.load(Ordering::Acquire) && !sender.is_closed() {
        if gate.epoch() != epoch {
            controls.set_enabled(false);
            // Drain events captured before activation without executing them.
            while let Some(event) = gilrs.next_event() {
                if gate.stopped.load(Ordering::Acquire) {
                    return;
                }
                process_event(event, &mut controls, &gate);
            }
            // Seed held buttons at startup/reconnection, including analog triggers.
            for (id, gamepad) in gilrs.gamepads() {
                for button in BUTTONS {
                    let value = gamepad.button_data(button).map_or(0.0, |data| data.value());
                    controls.button_value(
                        usize::from(id),
                        mapped_button(button).unwrap(),
                        value,
                        Instant::now(),
                    );
                }
            }
            epoch = gate.epoch();
            controls.set_enabled(epoch & 1 != 0);
        }

        // Nonblocking batches keep cancellation bounded even when gilrs filters
        // continuously discard noisy axis events. Only this worker waits.
        for _ in 0..256 {
            if gate.stopped.load(Ordering::Acquire) {
                return;
            }
            let Some(event) = gilrs.next_event() else {
                break;
            };
            if !gate.accepts(epoch) {
                controls.set_enabled(false);
            }
            if let Some(shortcut) = process_event(event, &mut controls, &gate) {
                send_shortcut(&sender, &gate, epoch, shortcut);
            }
        }
        if gate.accepts(epoch) {
            for shortcut in controls.repeats(Instant::now()) {
                send_shortcut(&sender, &gate, epoch, shortcut);
            }
        }
        gilrs.inc();
        std::thread::park_timeout(if epoch & 1 != 0 {
            INPUT_WAIT
        } else {
            Duration::from_millis(100)
        });
    }
}

fn process_event(
    event: Event,
    controls: &mut GamepadControls,
    gate: &InputGate,
) -> Option<PlaybackShortcut> {
    match event.event {
        // gilrs emits ButtonChanged for digital buttons too. Reading one event
        // family avoids duplicate edges and competing analog trigger thresholds.
        EventType::ButtonChanged(button, value, _) => controls.button_value(
            usize::from(event.id),
            mapped_button(button)?,
            value,
            Instant::now(),
        ),
        EventType::Connected => {
            controls.set_enabled(false);
            gate.invalidate();
            None
        }
        EventType::Disconnected => {
            controls.disconnect(usize::from(event.id));
            controls.set_enabled(false);
            gate.invalidate();
            None
        }
        _ => None,
    }
}

fn send_shortcut(
    sender: &Sender<GamepadMessage>,
    gate: &InputGate,
    epoch: u64,
    shortcut: PlaybackShortcut,
) {
    if gate.accepts(epoch) {
        // A stalled UI must not stall release/disconnection handling or collect
        // an unbounded backlog of seeks. Expired commands are also rejected by UI.
        let _ = sender.try_send(GamepadMessage {
            shortcut,
            epoch,
            created: Instant::now(),
        });
    }
}

const BUTTONS: [Button; 16] = [
    Button::South,
    Button::East,
    Button::West,
    Button::North,
    Button::DPadLeft,
    Button::DPadRight,
    Button::DPadUp,
    Button::DPadDown,
    Button::LeftTrigger,
    Button::RightTrigger,
    Button::Start,
    Button::Select,
    Button::LeftTrigger2,
    Button::RightTrigger2,
    Button::LeftThumb,
    Button::RightThumb,
];

fn mapped_button(button: Button) -> Option<GamepadButton> {
    Some(match button {
        Button::South => GamepadButton::South,
        Button::East => GamepadButton::East,
        Button::West => GamepadButton::West,
        Button::North => GamepadButton::North,
        Button::DPadLeft => GamepadButton::Left,
        Button::DPadRight => GamepadButton::Right,
        Button::DPadUp => GamepadButton::Up,
        Button::DPadDown => GamepadButton::Down,
        Button::LeftTrigger => GamepadButton::LeftShoulder,
        Button::RightTrigger => GamepadButton::RightShoulder,
        Button::Start => GamepadButton::Start,
        Button::Select => GamepadButton::Select,
        Button::LeftTrigger2 => GamepadButton::LeftTrigger,
        Button::RightTrigger2 => GamepadButton::RightTrigger,
        Button::LeftThumb => GamepadButton::LeftThumb,
        Button::RightThumb => GamepadButton::RightThumb,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_and_hotplug_invalidate_queued_actions() {
        let gate = InputGate::default();
        gate.set_active(true);
        let epoch = gate.epoch();
        assert!(gate.accepts(epoch));
        gate.set_active(false);
        gate.set_active(true);
        assert!(!gate.accepts(epoch));
        let epoch = gate.epoch();
        gate.invalidate();
        assert!(!gate.accepts(epoch));
        assert!(gate.accepts(gate.epoch()));
    }

    #[test]
    fn stale_and_stopped_delivery_is_rejected() {
        let input = GamepadInput {
            gate: Arc::new(InputGate::default()),
            wake: None,
        };
        input.set_active(true);
        let mut message = GamepadMessage {
            shortcut: PlaybackShortcut::TogglePlayback,
            epoch: input.gate.epoch(),
            created: Instant::now(),
        };
        assert!(input.accepts(&message));
        message.created -= MAX_DELIVERY_AGE + Duration::from_secs(1);
        assert!(!input.accepts(&message));
        let gate = input.gate.clone();
        drop(input);
        assert!(!gate.accepts(gate.epoch()));
    }

    #[test]
    fn dropping_input_wakes_a_waiting_worker() {
        let gate = Arc::new(InputGate::default());
        let worker_gate = gate.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            std::thread::park_timeout(Duration::from_secs(10));
            sender
                .send(worker_gate.stopped.load(Ordering::Acquire))
                .unwrap();
        });
        let input = GamepadInput {
            gate,
            wake: Some(worker.thread().clone()),
        };
        drop(input);
        assert_eq!(receiver.recv_timeout(Duration::from_secs(1)), Ok(true));
        worker.join().unwrap();
    }
}
