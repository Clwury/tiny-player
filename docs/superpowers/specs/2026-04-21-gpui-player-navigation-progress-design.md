# GPUI Player Navigation And Progress Design

## Context

The existing player already supports:

- scanning `sample/` at startup
- selecting videos from the playlist sidebar
- preloading the first video without autoplay
- play and pause
- embedded video rendering inside the GPUI window

The current control area is intentionally minimal. This extension adds the next layer of playback controls without expanding into a full media-player feature set.

## Goals

- Add `Previous` and `Next` controls to the bottom control strip.
- Add a read-only progress bar to the bottom control strip.
- Keep playlist switching behavior consistent with the first design: switching videos always loads the new video in paused state.
- Disable `Previous` on the first item and `Next` on the last item.
- Drive progress display from actual `libmpv2` playback state rather than UI-side timers.

## Non-Goals

- Seeking by dragging or clicking the progress bar.
- Automatic previous or next playback after a video ends.
- Looping from the first item to the last item or vice versa.
- Time labels, volume, fullscreen, keyboard shortcuts, or subtitle controls.

## User Experience

The player keeps the same overall layout:

- left playlist sidebar
- right video area
- bottom control strip

The bottom control strip becomes:

`Previous` | `Play/Pause` | `Next` | progress bar

Interaction rules:

1. Clicking `Previous` selects the prior playlist item, loads it, clears the old frame, and keeps playback paused.
2. Clicking `Next` selects the next playlist item, loads it, clears the old frame, and keeps playback paused.
3. `Previous` is disabled when the first item is selected.
4. `Next` is disabled when the last item is selected.
5. The progress bar is display-only. It shows the current playback position relative to total duration.
6. When duration is unavailable, the progress bar shows zero progress rather than an indeterminate or interactive state.
7. When a new file is selected, progress resets immediately while the new file loads.

## Architecture Changes

This extension should stay within the existing three main units.

### 1. `AppState`

Extend `AppState` with the UI-facing playback progress and navigation state.

New state should include:

- current playback position in seconds
- current media duration in seconds

New derived behavior should include:

- whether there is a previous item
- whether there is a next item
- a normalized progress value suitable for rendering the progress bar

`AppState` remains the place that answers UI questions such as:

- should `Previous` be enabled?
- should `Next` be enabled?
- how full should the progress bar be?

It should not calculate elapsed time on its own. It only reflects the latest known backend state.

### 2. `MpvBackend`

Extend `MpvBackend` to observe playback progress properties from `libmpv2`.

It should observe at least:

- current playback position, such as `time-pos`
- current media duration, such as `duration`

Those property changes should be converted into backend events, so the UI layer never reads mpv properties directly.

### 3. `PlayerApp`

`PlayerApp` continues to orchestrate interactions.

New responsibilities:

- render `Previous` and `Next` buttons in the bottom control strip
- disable boundary buttons using `AppState`
- handle button clicks by selecting adjacent playlist items
- reset progress-related state when loading a new file
- update progress state when backend events report new position and duration values
- render a read-only progress bar using the normalized progress value

No new module is required for this scope. Adding a separate control-bar state layer would be heavier than needed for these two features.

## Data Flow

### Startup

1. Startup behavior remains unchanged.
2. Once a file is loaded, mpv may emit duration and position values.
3. The backend forwards those property changes as events.
4. `PlayerApp` updates `AppState` and re-renders the progress bar.

### Previous / Next Navigation

1. The user clicks `Previous` or `Next`.
2. `PlayerApp` calculates the adjacent index from the current selection.
3. `AppState` updates the selected item.
4. `PlayerApp` clears the current rendered frame.
5. `PlayerApp` resets progress state to zero.
6. `PlayerApp` asks `MpvBackend` to load the new file.
7. `MpvBackend` loads the new file and forces paused state.
8. Later property updates repopulate duration and position for the new file.

### Progress Updates

1. `MpvBackend` receives property change events from mpv.
2. Position changes update the current playback position in `AppState`.
3. Duration changes update the known total duration in `AppState`.
4. `PlayerApp` re-renders the progress bar width based on normalized progress.

## UI Layout

The current title should remain visible above or adjacent to the video area as it is today.

The bottom control strip should be rearranged into a single compact row:

- `Previous` button
- `Play/Pause` button
- `Next` button
- progress bar filling the remaining horizontal space

Visual constraints:

- `Previous`, `Play/Pause`, and `Next` should have equal visual weight.
- Disabled navigation buttons must look disabled, not merely inert.
- The progress bar should expand to fill remaining width.
- The progress bar should remain clearly non-interactive in this version.

## Error Handling

This extension adds only a few new error-adjacent cases.

### Boundary navigation

- Reaching the first or last item is not an error.
- The corresponding button is disabled.

### Duration unavailable

- Not an error.
- The progress bar renders at zero progress until duration becomes known.

### Switching to a new item

- Old frame data must be cleared immediately.
- Old progress values must be reset immediately.
- If the new file fails to load, the error message should be shown and the stale frame must not remain visible.

## Testing Strategy

### Unit-testable state logic

Add or extend tests for:

- `has_previous` behavior at the first item
- `has_next` behavior at the last item
- middle-item navigation availability
- progress reset when the selected file changes
- normalized progress calculation from current position and total duration

### Backend tests

Add tests covering backend event mapping for:

- playback position property changes
- duration property changes

These tests should follow the existing `mpv_backend` style and use real property observation where the environment allows it.

### Manual verification

Manual checks should confirm:

- `Previous` is disabled on the first item
- `Next` is disabled on the last item
- clicking `Previous` or `Next` changes the selected item
- navigation leaves the new item paused
- the progress bar advances during playback
- the progress bar stops when paused
- progress resets when switching items

## Accepted Constraints

- Navigation remains playlist-order based.
- Navigation never loops.
- Switching items always returns to paused state.
- Progress display is read-only for this version.
- Timing values come from mpv property observation rather than UI-local clocks.

## Success Criteria

This extension is successful when:

- the bottom control strip includes `Previous`, `Play/Pause`, `Next`, and a progress bar
- boundary navigation buttons are visibly disabled at the ends of the playlist
- clicking `Previous` or `Next` selects the adjacent file and keeps it paused
- the progress bar reflects real playback progress during playback
- the progress bar stops moving when playback is paused
- switching files resets visible progress and does not leave stale video content behind
