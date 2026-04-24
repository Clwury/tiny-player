# GPUI Player Overlay Progress Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the read-only progress bar out of the bottom control strip and render it as a semi-transparent overlay inside the video area, slightly above the lower edge, without changing progress semantics.

**Architecture:** Keep this change local to `src/player_app.rs`. Reuse the existing `AppState::progress_fraction()` and backend event flow exactly as-is. Convert the video area into a layered container made of the current video/placeholder content plus an inset overlay progress track, and simplify the bottom control strip so it contains only the navigation and play/pause buttons.

**Tech Stack:** Rust 2024, GPUI 0.2.2, existing `PlayerApp` render tree, cargo test/check

---

## Planned File Structure

- `src/player_app.rs`
  The only production file that changes. It already owns the player layout, control strip, and the existing progress-bar rendering, so the overlay move should stay local here.

## Task 1: Add Overlay Layout Helpers

**Files:**
- Modify: `src/player_app.rs`

- [ ] **Step 1: Add failing helper tests for overlay progress sizing and inset behavior**

Append these tests to the existing `#[cfg(test)] mod tests` block in `src/player_app.rs`.

```rust
#[test]
fn overlay_progress_fill_fraction_stays_in_unit_interval() {
    assert_eq!(overlay_progress_fill_fraction(0.25), 0.25);
    assert_eq!(overlay_progress_fill_fraction(2.0), 1.0);
    assert_eq!(overlay_progress_fill_fraction(-1.0), 0.0);
}

#[test]
fn overlay_progress_bottom_inset_is_positive() {
    assert!(overlay_progress_bottom_inset_px() > 0.0);
}
```

- [ ] **Step 2: Run the focused player-app tests and verify they fail**

Run: `cargo test player_app`

Expected: FAIL because `overlay_progress_fill_fraction` and `overlay_progress_bottom_inset_px` do not exist yet.

- [ ] **Step 3: Implement the minimal overlay helper functions**

Add these helpers near the other free functions in `src/player_app.rs`.

```rust
fn overlay_progress_fill_fraction(progress_fraction: f32) -> f32 {
    progress_fraction.clamp(0.0, 1.0)
}

fn overlay_progress_bottom_inset_px() -> f32 {
    12.0
}

fn overlay_progress_horizontal_inset_px() -> f32 {
    14.0
}
```

- [ ] **Step 4: Run the focused player-app tests again**

Run: `cargo test player_app`

Expected: PASS.

- [ ] **Step 5: Commit the helper slice**

```bash
git add src/player_app.rs
git commit -m "feat: add overlay progress helpers"
```

## Task 2: Move The Progress Bar Into The Video Container Overlay

**Files:**
- Modify: `src/player_app.rs`

- [ ] **Step 1: Build the overlay progress bar container in `render()`**

Replace the existing progress bar construction in `src/player_app.rs` with an overlay-specific version that uses the new helper names.

```rust
        let progress_fill_fraction = overlay_progress_fill_fraction(self.state.progress_fraction());
        let overlay_progress_bar = div()
            .absolute()
            .left(px(overlay_progress_horizontal_inset_px()))
            .right(px(overlay_progress_horizontal_inset_px()))
            .bottom(px(overlay_progress_bottom_inset_px()))
            .h(px(8.0))
            .rounded_sm()
            .bg(gpui::rgba(0x00, 0x00, 0x00, 0.45))
            .child(
                div()
                    .h_full()
                    .w(gpui::relative(progress_fill_fraction))
                    .rounded_sm()
                    .bg(rgb(0x2f81f7)),
            );
```

- [ ] **Step 2: Layer the video container so the overlay belongs to it**

Change the video section of `render()` so the existing bordered video container becomes a relative positioned layered region.

Replace the current video container block:

```rust
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(0x30363d))
                            .p_2()
                            .child(video_area),
                    )
```

with:

```rust
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .overflow_hidden()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(0x30363d))
                            .p_2()
                            .child(video_area)
                            .child(overlay_progress_bar),
                    )
```

This keeps the overlay inside the video container instead of the control strip.

- [ ] **Step 3: Remove the progress bar from the bottom control strip**

Update the bottom control row so it contains only `Previous`, `Play/Pause`, and `Next`.

Replace this row tail:

```rust
                            .child(previous_button)
                            .child(playback_toggle)
                            .child(next_button)
                            .child(progress_bar),
```

with:

```rust
                            .child(previous_button)
                            .child(playback_toggle)
                            .child(next_button),
```

- [ ] **Step 4: Run compile verification for the layout change**

Run: `cargo check`

Expected: PASS.

- [ ] **Step 5: Commit the overlay layout slice**

```bash
git add src/player_app.rs
git commit -m "feat: move progress bar into video overlay"
```

## Task 3: Preserve Control Accessibility And Visual Stability

**Files:**
- Modify: `src/player_app.rs`

- [ ] **Step 1: Add a failing helper test for overlay-only control-strip composition**

Append this test to `src/player_app.rs`.

```rust
#[test]
fn bottom_control_strip_keeps_only_transport_buttons() {
    let labels = bottom_control_labels();

    assert_eq!(labels, ["Previous", "Play/Pause", "Next"]);
}
```

- [ ] **Step 2: Run the focused player-app tests and verify they fail**

Run: `cargo test player_app`

Expected: FAIL because `bottom_control_labels` does not exist.

- [ ] **Step 3: Implement the minimal helper and align the render tree to it**

Add this helper to `src/player_app.rs`.

```rust
fn bottom_control_labels() -> [&'static str; 3] {
    ["Previous", "Play/Pause", "Next"]
}
```

Use the helper as the source of truth for the bottom-strip composition intent in the test module only. Keep production rendering unchanged except for these two layout-safety tweaks if they are not already present:

```rust
                    .overflow_hidden()
```

on the right-side content column, and:

```rust
                            .flex_none()
```

on the bottom control row.

These ensure the video area yields space first when the window is shortened.

- [ ] **Step 4: Run the focused player-app tests again**

Run: `cargo test player_app`

Expected: PASS.

- [ ] **Step 5: Run full automated verification**

Run: `cargo test && cargo check`

Expected: PASS.

- [ ] **Step 6: Commit the final overlay adjustment**

```bash
git add src/player_app.rs
git commit -m "feat: keep controls stable with overlay progress"
```

## Task 4: Manual Verification Of Overlay Placement

**Files:**
- Modify: none unless a concrete runtime issue is found

- [ ] **Step 1: Run the player for manual verification**

Run: `cargo run`

Expected manual checks:

- The progress bar no longer appears in the bottom control strip.
- The progress bar appears inside the video container.
- It sits slightly above the lower edge of the video area.
- The dark semi-transparent track remains readable on bright and dark video frames.
- The bottom control strip still contains only `Previous`, `Play/Pause`, and `Next`.
- The bottom buttons remain clickable.
- Playback progress still advances and pauses correctly.
- Shrinking the window keeps the bottom control row visible and clickable.

- [ ] **Step 2: If manual verification exposes a layout issue, fix only that specific issue and rerun `cargo test && cargo check && cargo run`**

```bash
cargo test && cargo check && cargo run
```

- [ ] **Step 3: Commit the finished overlay placement feature**

```bash
git add src/player_app.rs
git commit -m "feat: add video overlay progress bar"
```
