# GPUI Player Navigation And Progress Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add previous/next navigation buttons and a read-only playback progress bar to the existing GPUI video player while keeping playlist switching paused and boundary buttons disabled.

**Architecture:** Extend the existing `AppState` with progress and navigation-derived state, extend `MpvBackend` to observe `time-pos` and `duration`, and update `PlayerApp` to render the extra bottom controls and synchronize progress on backend events. Keep the current three-part split (`state`, `mpv_backend`, `player_app`) instead of introducing a new control-bar layer.

**Tech Stack:** Rust 2024, GPUI 0.2.2, libmpv2 5.0.3, existing SDL2/glow render host, cargo test/check

---

## Planned File Structure

- `src/state.rs`
  Extend the UI-facing state with playback position, duration, derived progress, and derived previous/next availability.
- `tests/state.rs`
  Add pure state tests for navigation availability and progress-reset/progress-normalization behavior.
- `src/mpv_backend.rs`
  Observe `time-pos` and `duration`, add backend event variants, and test event mapping with real mpv property updates where the environment supports it.
- `src/player_app.rs`
  Add adjacent-item navigation helpers, reset progress on file switches, and render the bottom control strip with `Previous`, `Play/Pause`, `Next`, and a read-only progress bar.

## Task 1: Extend `AppState` For Navigation And Progress

**Files:**
- Modify: `src/state.rs`
- Modify: `tests/state.rs`

- [ ] **Step 1: Add failing state tests for navigation availability and progress**

Append these tests to `tests/state.rs`.

```rust
#[test]
fn navigation_availability_tracks_selected_index() {
    let mut state = AppState::from_playlist(vec![
        entry("alpha.mp4"),
        entry("beta.webm"),
        entry("gamma.mkv"),
    ]);

    assert!(!state.has_previous());
    assert!(state.has_next());

    state.select(1);

    assert!(state.has_previous());
    assert!(state.has_next());

    state.select(2);

    assert!(state.has_previous());
    assert!(!state.has_next());
}

#[test]
fn selecting_a_new_item_resets_progress() {
    let mut state = AppState::from_playlist(vec![entry("alpha.mp4"), entry("beta.webm")]);

    state.update_progress(12.5, 50.0);
    state.select(1);

    assert_eq!(state.playback_position_seconds, 0.0);
    assert_eq!(state.playback_duration_seconds, 0.0);
    assert_eq!(state.progress_fraction(), 0.0);
}

#[test]
fn progress_fraction_uses_position_and_duration() {
    let mut state = AppState::from_playlist(vec![entry("alpha.mp4")]);

    state.update_progress(25.0, 100.0);

    assert_eq!(state.progress_fraction(), 0.25);
}
```

- [ ] **Step 2: Run the state test suite and verify the new tests fail for the expected reason**

Run: `cargo test --test state`

Expected: FAIL with missing methods or fields such as `has_previous`, `has_next`, `update_progress`, or `progress_fraction`.

- [ ] **Step 3: Implement the minimal `AppState` extension**

Update `src/state.rs` to this shape.

```rust
use crate::media::PlaylistEntry;

pub struct AppState {
    pub playlist: Vec<PlaylistEntry>,
    pub selected_index: Option<usize>,
    pub is_playing: bool,
    pub error_message: Option<String>,
    pub playback_position_seconds: f64,
    pub playback_duration_seconds: f64,
}

impl AppState {
    pub fn from_playlist(playlist: Vec<PlaylistEntry>) -> Self {
        let selected_index = (!playlist.is_empty()).then_some(0);

        Self {
            playlist,
            selected_index,
            is_playing: false,
            error_message: None,
            playback_position_seconds: 0.0,
            playback_duration_seconds: 0.0,
        }
    }

    pub fn current_entry(&self) -> Option<&PlaylistEntry> {
        self.selected_index
            .and_then(|index| self.playlist.get(index))
    }

    pub fn current_title(&self) -> &str {
        self.current_entry()
            .map(|entry| entry.display_name.as_str())
            .unwrap_or("No video selected")
    }

    pub fn select(&mut self, index: usize) {
        if self.selected_index == Some(index) {
            return;
        }

        if self.playlist.get(index).is_some() {
            self.selected_index = Some(index);
            self.is_playing = false;
            self.playback_position_seconds = 0.0;
            self.playback_duration_seconds = 0.0;
            self.clear_error();
        }
    }

    pub fn sync_pause_state(&mut self, paused: bool) {
        self.is_playing = self.can_control_playback() && !paused;
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.is_playing = false;
        self.error_message = Some(message.into());
    }

    pub fn clear_error(&mut self) {
        self.error_message = None;
    }

    pub fn can_control_playback(&self) -> bool {
        self.current_entry().is_some()
    }

    pub fn has_previous(&self) -> bool {
        matches!(self.selected_index, Some(index) if index > 0)
    }

    pub fn has_next(&self) -> bool {
        matches!(self.selected_index, Some(index) if index + 1 < self.playlist.len())
    }

    pub fn update_progress(&mut self, position_seconds: f64, duration_seconds: f64) {
        self.playback_position_seconds = position_seconds.max(0.0);
        self.playback_duration_seconds = duration_seconds.max(0.0);
    }

    pub fn update_position(&mut self, position_seconds: f64) {
        self.playback_position_seconds = position_seconds.max(0.0);
    }

    pub fn update_duration(&mut self, duration_seconds: f64) {
        self.playback_duration_seconds = duration_seconds.max(0.0);
    }

    pub fn progress_fraction(&self) -> f32 {
        if self.playback_duration_seconds <= 0.0 {
            0.0
        } else {
            (self.playback_position_seconds / self.playback_duration_seconds)
                .clamp(0.0, 1.0) as f32
        }
    }
}
```

- [ ] **Step 4: Run the state tests again**

Run: `cargo test --test state`

Expected: PASS with all state tests green.

- [ ] **Step 5: Commit the state extension**

```bash
git add src/state.rs tests/state.rs
git commit -m "feat: add player navigation and progress state"
```

## Task 2: Extend `MpvBackend` With Position And Duration Events

**Files:**
- Modify: `src/mpv_backend.rs`

- [ ] **Step 1: Add failing backend tests for progress-related property events**

Append these tests to `src/mpv_backend.rs` inside the existing `#[cfg(test)] mod tests` block.

```rust
#[test]
fn poll_events_reports_duration_changes() {
    let mut backend = MpvBackend::new().unwrap();
    let path = sample_video_path();

    backend.load_file(&path).unwrap();

    let events = wait_for_events_until("duration update", &mut backend, |events| {
        events
            .iter()
            .any(|event| matches!(event, BackendEvent::DurationChanged(duration) if *duration > 0.0))
    });

    assert!(events
        .iter()
        .any(|event| matches!(event, BackendEvent::DurationChanged(duration) if *duration > 0.0)));
}

#[test]
fn poll_events_reports_position_changes_after_playback_starts() {
    let mut backend = MpvBackend::new().unwrap();
    let path = sample_video_path();

    backend.load_file(&path).unwrap();
    backend.poll_events();
    assert!(backend.toggle_playback().unwrap());

    let events = wait_for_events_until("position update", &mut backend, |events| {
        events
            .iter()
            .any(|event| matches!(event, BackendEvent::PositionChanged(position) if *position >= 0.0))
    });

    assert!(events
        .iter()
        .any(|event| matches!(event, BackendEvent::PositionChanged(position) if *position >= 0.0)));
}
```

- [ ] **Step 2: Run the backend test suite and verify the new tests fail for the expected reason**

Run: `cargo test mpv_backend`

Expected: FAIL with missing `BackendEvent::DurationChanged`, `BackendEvent::PositionChanged`, or missing property observation logic.

- [ ] **Step 3: Extend backend event observation and mapping**

Update `src/mpv_backend.rs` with these focused changes.

1. Extend `BackendEvent`.

```rust
#[derive(Debug)]
pub enum BackendEvent {
    Pause(bool),
    FileTitle(String),
    PositionChanged(f64),
    DurationChanged(f64),
    LoadFailed(String),
    Fatal(String),
}
```

2. Observe the extra mpv properties in `new()`.

```rust
        mpv.observe_property("pause", Format::Flag, 0)
            .map_err(BackendError::from)?;
        mpv.observe_property("media-title", Format::String, 1)
            .map_err(BackendError::from)?;
        mpv.observe_property("time-pos", Format::Double, 2)
            .map_err(BackendError::from)?;
        mpv.observe_property("duration", Format::Double, 3)
            .map_err(BackendError::from)?;
```

3. Map the property changes in `poll_events()`.

```rust
                Ok(Event::PropertyChange {
                    name: "time-pos",
                    change: PropertyData::Double(position),
                    ..
                }) => events.push(BackendEvent::PositionChanged(position)),
                Ok(Event::PropertyChange {
                    name: "duration",
                    change: PropertyData::Double(duration),
                    ..
                }) => events.push(BackendEvent::DurationChanged(duration)),
```

- [ ] **Step 4: Run the backend tests again**

Run: `cargo test mpv_backend`

Expected: PASS with the existing backend tests plus the new duration/position tests green.

- [ ] **Step 5: Commit the backend progress event slice**

```bash
git add src/mpv_backend.rs
git commit -m "feat: observe player progress from mpv"
```

## Task 3: Add Adjacent Navigation Helpers To `PlayerApp`

**Files:**
- Modify: `src/player_app.rs`

- [ ] **Step 1: Add failing helper tests for adjacent navigation**

Append these tests to the existing `#[cfg(test)] mod tests` block in `src/player_app.rs`.

```rust
#[test]
fn previous_index_is_none_for_first_item() {
    assert_eq!(adjacent_index(Some(0), 3, Direction::Previous), None);
}

#[test]
fn next_index_is_none_for_last_item() {
    assert_eq!(adjacent_index(Some(2), 3, Direction::Next), None);
}

#[test]
fn adjacent_index_moves_within_playlist_bounds() {
    assert_eq!(adjacent_index(Some(1), 3, Direction::Previous), Some(0));
    assert_eq!(adjacent_index(Some(1), 3, Direction::Next), Some(2));
}
```

- [ ] **Step 2: Run the focused player-app tests and verify they fail**

Run: `cargo test player_app::tests -- --exact`

Expected: FAIL because `Direction` and `adjacent_index` do not exist yet.

- [ ] **Step 3: Implement the minimal adjacent navigation helpers and handlers**

Update `src/player_app.rs` with these additions near the existing helper functions.

```rust
#[derive(Clone, Copy)]
enum Direction {
    Previous,
    Next,
}

fn adjacent_index(selected_index: Option<usize>, len: usize, direction: Direction) -> Option<usize> {
    match (selected_index, direction) {
        (Some(index), Direction::Previous) if index > 0 => Some(index - 1),
        (Some(index), Direction::Next) if index + 1 < len => Some(index + 1),
        _ => None,
    }
}
```

Add this method to `impl PlayerApp`.

```rust
    fn select_adjacent_item(&mut self, direction: Direction, cx: &mut Context<Self>) {
        let Some(index) = adjacent_index(
            self.state.selected_index,
            self.state.playlist.len(),
            direction,
        ) else {
            return;
        };

        self.select_playlist_item(index, cx);
    }
```

- [ ] **Step 4: Run the focused player-app tests again**

Run: `cargo test player_app::tests`

Expected: PASS.

- [ ] **Step 5: Commit the navigation helper slice**

```bash
git add src/player_app.rs
git commit -m "feat: add adjacent playlist navigation helpers"
```

## Task 4: Render The Extended Bottom Control Strip

**Files:**
- Modify: `src/player_app.rs`

- [ ] **Step 1: Add a failing test for progress-bar fraction clamping**

Append this test to the existing `src/player_app.rs` test module.

```rust
#[test]
fn clamped_progress_fraction_stays_within_zero_and_one() {
    assert_eq!(clamped_progress_fraction(0.25), 0.25);
    assert_eq!(clamped_progress_fraction(2.0), 1.0);
    assert_eq!(clamped_progress_fraction(-1.0), 0.0);
}
```

- [ ] **Step 2: Run the focused player-app tests and verify they fail**

Run: `cargo test player_app::tests`

Expected: FAIL because `clamped_progress_fraction` does not exist.

- [ ] **Step 3: Implement the control-strip rendering changes**

Add this helper near the other free functions in `src/player_app.rs`.

```rust
fn clamped_progress_fraction(progress_fraction: f32) -> f32 {
    progress_fraction.clamp(0.0, 1.0)
}
```

Then update `render()` in `src/player_app.rs` with these changes:

1. Compute navigation/button state and progress.

```rust
        let can_toggle_playback = self.backend.is_some() && self.state.can_control_playback();
        let can_go_previous = self.backend.is_some() && self.state.has_previous();
        let can_go_next = self.backend.is_some() && self.state.has_next();
        let progress_fraction = clamped_progress_fraction(self.state.progress_fraction());
```

2. Add `Previous` and `Next` buttons using the same visual weight as the play button.

```rust
        let mut previous_button = div()
            .id("playback-previous")
            .px_4()
            .py_2()
            .rounded_sm()
            .bg(if can_go_previous { rgb(0x3a6ea5) } else { rgb(0x3a434d) })
            .child("Previous");

        previous_button = if can_go_previous {
            previous_button
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.select_adjacent_item(Direction::Previous, cx);
                }))
        } else {
            previous_button.cursor_default()
        };

        let mut next_button = div()
            .id("playback-next")
            .px_4()
            .py_2()
            .rounded_sm()
            .bg(if can_go_next { rgb(0x3a6ea5) } else { rgb(0x3a434d) })
            .child("Next");

        next_button = if can_go_next {
            next_button
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.select_adjacent_item(Direction::Next, cx);
                }))
        } else {
            next_button.cursor_default()
        };
```

3. Replace the bottom status-only strip with a two-part bottom section: message bar plus compact controls row with progress fill.

```rust
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(previous_button)
                            .child(playback_toggle)
                            .child(next_button)
                            .child(
                                div()
                                    .flex_1()
                                    .h(px(10.0))
                                    .rounded_full()
                                    .bg(rgb(0x2a2f37))
                                    .child(
                                        div()
                                            .h_full()
                                            .rounded_full()
                                            .bg(rgb(0x58a6ff))
                                            .w(relative(progress_fraction)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .rounded_sm()
                            .bg(rgb(0x1f242d))
                            .text_color(rgb(0x9fb3c8))
                            .child(message_text),
                    ),
```

Keep the row order as `Previous`, `Play/Pause`, `Next`, progress bar.

- [ ] **Step 4: Run compile verification for the UI shell**

Run: `cargo check`

Expected: PASS.

- [ ] **Step 5: Commit the control-strip rendering slice**

```bash
git add src/player_app.rs
git commit -m "feat: add bottom navigation and progress controls"
```

## Task 5: Wire Backend Progress Events Into The App Loop

**Files:**
- Modify: `src/player_app.rs`

- [ ] **Step 1: Add a failing helper test for reset-on-switch behavior**

Append this test to `src/player_app.rs`.

```rust
#[test]
fn reset_progress_clears_position_and_duration() {
    let mut state = crate::state::AppState::from_playlist(vec![crate::media::PlaylistEntry {
        path: std::path::PathBuf::from("alpha.mp4"),
        display_name: "alpha.mp4".to_string(),
    }]);
    state.update_progress(5.0, 10.0);

    reset_progress(&mut state);

    assert_eq!(state.playback_position_seconds, 0.0);
    assert_eq!(state.playback_duration_seconds, 0.0);
}
```

- [ ] **Step 2: Run the focused player-app tests and verify they fail**

Run: `cargo test player_app::tests`

Expected: FAIL because `reset_progress` does not exist.

- [ ] **Step 3: Implement the progress event handling and switch reset path**

Add this helper near the other free functions in `src/player_app.rs`.

```rust
fn reset_progress(state: &mut AppState) {
    state.update_progress(0.0, 0.0);
}
```

Update `poll_backend()` to handle the new backend events.

```rust
                    BackendEvent::PositionChanged(position) => self.state.update_position(position),
                    BackendEvent::DurationChanged(duration) => self.state.update_duration(duration),
```

Update `select_playlist_item()` so file switches reset progress before loading the new file.

```rust
        clear_current_frame(&mut self.current_frame);
        reset_progress(&mut self.state);
        self.current_title = self.state.current_title().to_owned();
```

Also reset progress when a file load fails.

```rust
                clear_current_frame(&mut self.current_frame);
                reset_progress(&mut self.state);
                self.state.set_error(error.to_string());
```

- [ ] **Step 4: Run full automated verification**

Run: `cargo test && cargo check`

Expected: PASS.

- [ ] **Step 5: Commit the event-to-UI synchronization slice**

```bash
git add src/player_app.rs
git commit -m "feat: sync player progress into controls"
```

## Task 6: Manual Verification Of Navigation And Progress

**Files:**
- Modify: none unless runtime issues are found

- [ ] **Step 1: Run the full test suite again from a clean state**

Run: `cargo test`

Expected: PASS with all unit and integration tests green.

- [ ] **Step 2: Run the player for manual verification**

Run: `cargo run`

Expected manual checks:

- The bottom strip shows `Previous`, `Play/Pause`, `Next`, and a progress bar.
- `Previous` is disabled on the first playlist item.
- `Next` is disabled on the last playlist item.
- Clicking `Next` changes the selected file and leaves playback paused.
- Clicking `Previous` changes the selected file and leaves playback paused.
- During playback, the progress bar advances visibly.
- When paused, the progress bar stops advancing.
- After switching files, the progress bar resets and stale video content is not shown.

- [ ] **Step 3: If manual verification exposes issues, fix only the specific failing behavior and rerun `cargo test && cargo check && cargo run`**

```bash
cargo test && cargo check && cargo run
```

- [ ] **Step 4: Commit the finished navigation/progress feature**

```bash
git add src/state.rs src/mpv_backend.rs src/player_app.rs tests/state.rs
git commit -m "feat: add player navigation and progress bar"
```
