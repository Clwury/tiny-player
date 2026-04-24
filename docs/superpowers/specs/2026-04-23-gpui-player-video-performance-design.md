# GPUI Player Video Rendering Performance Design

## Context

The current player already supports:

- scanning `sample/` at startup
- selecting videos from the playlist sidebar
- preloading the first item without autoplay
- play and pause
- embedded video rendering inside the GPUI window

Today the embedded video path is implemented by rendering `libmpv` output into a hidden SDL OpenGL context, reading the framebuffer back into CPU memory, converting that buffer into `gpui::RenderImage`, and then displaying it through `img(...)`.

That architecture made the first version straightforward, but it now defines the performance ceiling of the player.

## Current Bottlenecks

The current rendering path has four main costs:

1. `libmpv` renders into an offscreen OpenGL framebuffer.
2. `glReadPixels` copies the full frame from GPU memory back to CPU memory.
3. A fresh `RenderImage` is created for every frame.
4. GPUI uploads that frame data back into its atlas because each new `RenderImage` has a new image id.

This means every video frame effectively travels through a `GPU -> CPU -> GPU` path.

The current player also polls rendering on a fixed timer, so unchanged or paused frames still pay unnecessary presentation cost.

The current target desktop environment is Wayland, so the next implementation plan must optimize the existing embeddable path that already works there instead of depending on an X11-only native child-window presentation strategy.

## Goals

- Raise video playback performance materially on the current Wayland desktop environment.
- Stop repeated rendering work when no new frame is available.
- Reduce per-frame allocation churn and image-atlas churn on the existing embeddable path.
- Preserve the existing playlist, title, playback controls, and mpv event-driven state updates.
- Keep the existing GPUI-embedded video presentation working on Wayland.
- Structure the player so a future native-surface backend can be added without rewriting `PlayerApp` again.

## Non-Goals

- Delivering cross-platform parity in the same redesign.
- Solving Wayland-native zero-copy embedding in this implementation plan.
- Delivering an X11 or XWayland child-window backend in this implementation plan.
- Adding new playback features such as seeking, subtitles, fullscreen choreography, volume, or keyboard shortcuts.
- Reworking the playlist or control-strip UX beyond changes required by the new video presentation path.
- Building a GPUI-internal custom renderer that depends on non-public GPUI rendering internals.

## Selected Approach

The redesign should be implemented in two concrete phases for the current implementation plan, with native-surface work deferred.

### Phase 0: Stop The Current Path From Wasting Work

The current readback path should be kept temporarily, but hardened so it does not do needless work:

- use mpv render update notifications instead of unconditional fixed-rate frame pulls
- skip frame readback when there is no new frame or redraw request
- stop re-rendering unchanged paused frames
- reuse the pixel buffer instead of allocating a fresh `Vec<u8>` every frame
- explicitly release old GPUI images so atlas entries do not accumulate indefinitely

This phase does not remove the architectural bottleneck, but it immediately lowers idle CPU usage, GPU upload churn, and long-session memory growth.

### Phase 1: Make The Optimized Readback Path The Supported Wayland Backend

For the current Wayland environment, the supported backend should remain the embeddable readback path, but it should become event-driven and resource-stable.

This phase should:

- keep video embedded directly in the GPUI layout on Wayland
- drive frame presentation from mpv render update notifications rather than unconditional polling
- track whether a new frame is actually dirty before performing readback
- avoid repeated atlas growth by explicitly releasing replaced images
- reuse rendering buffers and avoid repeated size churn when dimensions did not change

This phase does not remove the fundamental `GPU -> CPU -> GPU` path, but it is the highest-confidence route for the user's current environment and should deliver the immediate practical gains.

### Phase 2: Prepare Clean Boundaries For Future Native Backends

The current implementation plan should introduce a small presentation boundary so the readback path is no longer hard-wired into `PlayerApp`.

That boundary exists so future work can add either:

- an X11 or XWayland child-window backend
- a Wayland-specific native presentation strategy

Neither native backend should be part of this implementation plan.

## Accepted Constraints

- The current target environment is native Wayland.
- The supported backend for this implementation plan remains the optimized readback path.
- A future native backend must be treated as follow-up work, not hidden inside this plan.
- The redesign must provide measurable wins without requiring XWayland.
- The redesign should avoid coupling to private GPUI renderer internals.

## Architecture Changes

### 1. Introduce A Small Video Presentation Boundary

`PlayerApp` should stop knowing the details of how a video frame becomes visible.

Instead, the player should talk to a small presentation boundary with responsibilities such as:

- initialize the active video presentation backend
- react to viewport size or position changes
- present video through the active backend
- release backend-specific resources when switching modes or shutting down

This boundary should stay narrow. It exists to separate UI orchestration from video presentation strategy, not to create a deep abstraction hierarchy.

### 2. Keep `MpvBackend` Focused On Playback And Events

`MpvBackend` should remain the place that:

- configures mpv
- loads files
- toggles playback
- observes pause, title, position, and duration changes
- exposes mpv access needed by the active presentation backend

It should not absorb layout logic or GPUI-specific window positioning.

### 3. Keep One Concrete Backend In Scope

This implementation plan should optimize one concrete backend: the existing SDL/OpenGL readback host.

Responsibilities:

- own the hidden OpenGL context used by `libmpv` render API
- listen for mpv render update notifications
- render only when mpv reports a new frame or redraw
- reuse readback buffers across frames
- convert frames to `RenderImage` only when needed
- release replaced images from GPUI explicitly

The presentation boundary introduced in this redesign should make it possible to add native backends later, but those native backends are not part of this spec's implementation scope.

### 4. Track The Video Viewport Explicitly

The current readback path renders to a size derived from the overall window viewport. The optimized Wayland path should instead track the intended video region explicitly so presentation work follows the actual video area and so future backends have a clean source of truth.

The UI should therefore expose the computed bounds of the video region explicitly.

That viewport state should be used for:

- sizing the readback target more accurately
- deciding when rendering should be skipped because the effective video area is invalid
- providing a future handoff point for native backends
- avoiding redundant resize work when bounds have not changed

### 5. Treat GPUI As The Shell, Not The Video Renderer

For the current Wayland-first path, GPUI should continue to render:

- playlist sidebar
- title text
- control strip
- status or error messages
- placeholder background when no video is active

The redesign should reduce unnecessary use of GPUI's sprite atlas, but the current implementation plan still displays video through GPUI images.

## Data Flow

### Startup

1. `PlayerApp` discovers the playlist and builds the initial state.
2. `MpvBackend` is created and configured as it is today.
3. The presentation boundary initializes the optimized readback backend.
4. The first media item is loaded in paused state as it is today.

### Viewport Changes

1. GPUI computes the layout for the video region.
2. `PlayerApp` observes the current video bounds.
3. If the bounds changed, the presentation backend is notified.
4. The active backend updates its target size only when the effective dimensions changed.

### Playback In Readback Mode

1. mpv signals that a frame or redraw is available.
2. The readback backend marks itself dirty.
3. `PlayerApp` asks the backend to present only when dirty.
4. The backend renders the frame offscreen, reads pixels back, updates the visible GPUI image, and releases the replaced image.
5. GPUI displays the most recent image.

### File Switching

1. The user selects a different playlist entry.
2. `PlayerApp` updates selection and resets progress state.
3. The active presentation backend clears any stale visible content for the old file.
4. `MpvBackend` loads the new file in paused state.
5. Later mpv events repopulate title and timing data.

## Deferred Follow-Up Work

Once the Wayland-first optimized backend is stable, separate follow-up design work should evaluate:

- an X11 or XWayland child-window backend for higher performance in compatible sessions
- a Wayland-native presentation strategy that avoids the current readback path
- video overlay controls that sit above a native video surface

Those follow-up efforts should not be bundled into the current implementation plan.

## UI And Layout Impact

The overall UI structure should remain recognizable:

- left playlist sidebar
- right media area
- bottom control strip

The right media area may continue to use a GPUI `img(...)` element in this implementation plan, but `PlayerApp` should stop baking that assumption directly into its control flow. The image presentation details should live behind the presentation boundary.

For the optimized Wayland backend:

- the current embedded-video appearance remains available
- status text and placeholder content remain GPUI-rendered when no playable video is active
- the video region continues to participate naturally in GPUI layout

## Error Handling

### Readback backend unavailable

- Not a fatal application error.
- The player should show a clear initialization error if the existing embeddable backend cannot be created.
- The UI should expose a status or error message rather than silently leaving an empty video area.

### Render update notifications do not indicate new work

- Not an error.
- The backend should skip readback and retain the current visible frame.

### Viewport temporarily invalid

- Zero or negative effective dimensions are not fatal.
- The active backend should hide or skip presentation until valid bounds are available.

### File load failure

- Existing behavior should remain: stale progress and stale video must not remain visible as if playback succeeded.
- The backend should clear the visible GPUI image for the failed file rather than continuing to display the old frame.

### Long sessions

- Replaced GPUI images in readback mode must be explicitly released.
- Rendering buffers should be reused rather than repeatedly reallocated.

## Testing Strategy

### Unit-testable logic

Add or extend pure tests for:

- whether a dirty frame should be presented
- viewport normalization and no-op resize checks
- render update bookkeeping based on mpv notifications
- safe handling of zero-sized or invalid viewport states

### Backend-focused verification

Readback backend checks should confirm:

- paused playback does not continuously force new frame uploads
- new frames are only read back when mpv reports update work
- replaced images are released instead of accumulating indefinitely
- pixel buffers are reused instead of recreated every frame

### Compile verification

- `cargo check`
- `cargo test`

### Manual verification

Manual checks should confirm:

- normal playback still works after startup
- paused playback does not consume visibly unnecessary CPU relative to the current build
- resizing the window keeps the visible video aligned with its intended region
- switching files does not leave stale video visible
- long playback sessions do not show unbounded image-atlas or memory growth
- the player continues to function correctly on the current Wayland desktop session

### Performance validation

Performance validation should compare at least the following scenarios against the current implementation:

- paused video with the player left open for an extended period
- active playback at the default window size
- active playback after enlarging the window substantially

The redesign should be considered successful only if the new path measurably reduces idle work and frame churn on the current Wayland desktop session.

## Success Criteria

This redesign is successful when:

- frame presentation is driven by mpv update notifications instead of unconditional timer-driven redraws
- paused playback stops doing repeated frame work when no new frame is available
- the readback path reuses buffers and no longer leaks atlas entries during long sessions
- the playlist, title, play/pause, and progress behavior remain correct
- the current Wayland desktop session sees practical performance improvement without changing visible playback behavior
