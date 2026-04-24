# GPUI Player Overlay Progress Design

## Context

The current navigation/progress branch already adds:

- `Previous` and `Next` buttons
- a bottom control strip
- a read-only playback progress bar
- progress updates driven by `libmpv2` playback properties

The next adjustment is purely presentational: move the progress bar out of the bottom control strip and place it as an overlay inside the video area, slightly above the lower video boundary.

## Goals

- Move the progress bar from the bottom control strip into the video container.
- Position it slightly above the lower edge of the video area.
- Use a semi-transparent track so the bar remains visible on both dark and bright video frames.
- Keep the progress bar read-only.
- Keep `Previous`, `Play/Pause`, and `Next` in the bottom control strip.

## Non-Goals

- Changing how progress values are calculated.
- Changing how mpv duration or position updates are observed.
- Adding seek, drag, click, timestamps, hover effects, or thumbnail preview.
- Redesigning the rest of the player layout.

## User Experience

The right side of the player becomes visually split into three layers:

1. title area above the video container
2. video container with an overlay progress bar inside it
3. bottom control strip containing only `Previous`, `Play/Pause`, and `Next`

The progress bar should no longer appear in the bottom control strip.

Instead, it appears inside the video container:

- horizontally inset from the left and right edges
- vertically offset a small amount above the bottom edge
- visible regardless of whether the video frame is dark or bright

## Layout Design

### Video Container

The video container should become a layered region:

- base layer: current video frame or empty-state placeholder
- overlay layer: read-only progress bar positioned near the bottom

The overlay should belong to the video container, not to the page-level control strip.

### Progress Overlay

The overlay should have:

- a semi-transparent dark track
- a brighter filled portion representing progress
- a thin visual profile with no draggable thumb
- a small bottom inset so it feels suspended above the lower edge rather than glued to it

### Bottom Control Strip

The bottom control strip should now contain only:

- `Previous`
- `Play/Pause`
- `Next`

No progress bar should remain in that row.

## Architecture Impact

This change should remain local to `src/player_app.rs`.

No changes are needed to:

- `src/state.rs`
- `src/mpv_backend.rs`
- backend event mapping
- progress calculation logic

The existing `AppState::progress_fraction()` should continue to be the single source for the rendered fill amount.

## Data Flow

Data flow remains unchanged:

1. `MpvBackend` emits duration and position updates.
2. `PlayerApp` updates `AppState`.
3. `AppState::progress_fraction()` produces the normalized fill value.
4. `PlayerApp::render()` applies that value to the overlay progress bar width.

The only difference is where the progress bar is rendered.

## Error And Empty-State Behavior

### Normal playback

- The overlay track remains visible.
- The filled progress portion advances with playback and stops while paused.

### Duration unknown

- The overlay track remains visible.
- The filled progress portion remains at zero width.

### Error state

- The track may remain visible for layout stability.
- The filled portion should remain at zero width once progress has been reset.

### Empty or placeholder video area

- The overlay remains positioned inside the video container.
- It should not push layout around or move the bottom control strip.

## Visual Constraints

- The overlay must not obscure or replace the bottom control strip.
- The overlay must not block button clicks in the lower control strip.
- The overlay should preserve some horizontal inset from the container edges.
- The overlay should preserve a small vertical gap above the lower video edge.
- When the window becomes shorter, the bottom controls must remain visible and clickable; the video area should be the part that yields space.

## Testing Strategy

### Unit-testable logic

If any new helper is introduced for overlay inset or width normalization, add a small pure unit test for it.

### Compile verification

- `cargo check`

### Manual verification

Manual checks should confirm:

- the progress bar is no longer in the bottom control strip
- the progress bar appears inside the video container
- the progress bar sits slightly above the lower video edge
- the semi-transparent track stays visible on varying video content
- the bottom control strip still contains only `Previous`, `Play/Pause`, and `Next`
- buttons remain clickable after the layout move
- playback progress still advances and pauses correctly

## Success Criteria

This UI adjustment is successful when:

- the progress bar is rendered as an overlay within the video area
- the progress bar is positioned slightly above the lower edge of the video container
- the overlay uses a semi-transparent track and a visible filled portion
- the bottom control strip contains only navigation and play/pause controls
- the move does not change progress semantics or break control accessibility
