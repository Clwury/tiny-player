# GPUI + libmpv2 Video Player Design

## Context

The repository is currently a minimal Rust application with an empty dependency list, a trivial `src/main.rs`, and a `sample/` directory containing two local video files. The goal is to turn that scaffold into a Linux-first desktop video player built with `gpui` for the application shell and `libmpv2` for playback.

The player only needs a focused first version:

- play videos from the repository's `sample/` directory
- switch videos by clicking a playlist item
- play and pause the current video
- show the current file name

The product should prefer clear boundaries and predictable behavior over feature breadth.

## Goals

- Launch as a desktop application on Linux.
- Scan the top level of `sample/` on startup and build a playlist from supported video files.
- Show a playlist sidebar and a main video area in a single GPUI window.
- Select and preload the first video on startup without autoplay.
- Let the user click an item in the sidebar to switch the current video.
- Let the user play or pause the selected video with a single control button.
- Keep the application running when a single file fails to load, and surface a visible error message.

## Non-Goals

- Recursive media scanning outside the top level of `sample/`.
- Seek, volume, fullscreen, subtitles, playback speed, hotkeys, or automatic next-video playback.
- Cross-platform guarantees for the first version.
- Automated GUI end-to-end testing of rendered video output.

## User Experience

When the app starts, it scans `sample/` for video files and renders a two-pane layout.

- Left pane: playlist sidebar containing the discovered files.
- Right pane: video render area.
- Control strip: current file name and a single play/pause button.

Startup behavior is deterministic:

1. Load the playlist.
2. Select the first item if one exists.
3. Ask `libmpv2` to load that file.
4. Keep playback paused until the user presses play.

Interaction rules are intentionally strict for the first version:

- Clicking a playlist item switches the selected video immediately.
- After a switch, the new video is loaded but remains paused.
- Clicking play toggles playback on for the selected video.
- Clicking pause toggles playback off for the selected video.
- Clicking the already selected playlist item is a no-op.

## Architecture

The application should be split into a small number of focused units.

### 1. `AppState`

Owns the UI-facing state:

- playlist entries
- selected index
- current playback flag
- current file display name
- transient user-visible error message

`AppState` does not talk to `libmpv2` directly. It represents what the UI should render.

### 2. `MpvBackend`

Owns all `libmpv2` interaction:

- player initialization
- file loading
- play and pause commands
- observing playback-related state and error events

This boundary keeps raw mpv commands and lifecycle handling out of the GPUI view code.

### 3. GPUI view layer

Owns window construction and user interaction:

- rendering the playlist
- rendering the control strip
- hosting the video surface in the main pane
- dispatching user actions to the backend bridge
- applying backend state updates back into `AppState`

### 4. Playlist discovery helper

A small startup helper is enough for media discovery. It scans the top level of `sample/`, keeps regular files whose extensions are `.mp4`, `.mkv`, `.webm`, or `.mov`, sorts them by display name for deterministic ordering, and returns the resulting playlist entries. This does not need a long-lived service in the first version.

## Component Boundaries

The design should preserve these rules:

- The view layer can request actions such as `load(index)` or `toggle_playback()`, but it does not construct mpv commands itself.
- `MpvBackend` can publish state changes and errors, but it does not decide how they are rendered.
- Playlist scanning happens at startup and feeds initial state; it does not stay active or watch the filesystem.

This keeps the first version small while still leaving room for later additions like progress reporting or keyboard shortcuts.

## Data Flow

### Startup

1. The app scans `sample/`.
2. The resulting entries populate `AppState.playlist`.
3. If the playlist is non-empty, `AppState.selected_index` becomes `0`.
4. The UI requests `MpvBackend` to load the first file.
5. `MpvBackend` loads the file in paused state.
6. The UI reflects the selected file name and paused status.

### Play/Pause

1. The user clicks the play/pause button.
2. The UI dispatches `toggle_playback()`.
3. `MpvBackend` issues the corresponding mpv command.
4. The backend reports the resulting playback state.
5. `AppState.is_playing` updates and the button label changes.

### Video Switch

1. The user clicks a playlist item.
2. The UI updates `selected_index`.
3. The UI requests `MpvBackend` to load that file.
4. The backend loads the new file and forces paused state.
5. `AppState.is_playing` becomes `false`.
6. The file name display updates to the newly selected item.

## Rendering Strategy

The right-side video pane should be a dedicated host area for mpv-backed rendering inside the GPUI window rather than a separate standalone player window. The implementation should obtain the native window or surface handle required by `libmpv2` and bind mpv's rendering context to that pane.

This keeps the visible result aligned with the requested product shape: one desktop window with a playlist sidebar and an embedded playback surface.

## Error Handling

The first version only handles the errors that matter for the requested workflow.

### `sample/` missing or unreadable

- The app still opens.
- The playlist pane shows a clear error.
- The play/pause control is disabled.

### No supported videos found

- The app still opens.
- The playlist is empty.
- The video area remains blank or black.
- The UI shows a "no videos found" message.

### `libmpv2` initialization failure

- The app surfaces a clear backend initialization error.
- Playback controls remain disabled.
- The UI must not pretend a player is available.

### Per-file load failure

- The app keeps running.
- The selected item remains selected.
- The UI shows an inline error for that failed load.
- The user can still choose another file.

## Testing Strategy

The first version should combine targeted unit tests with manual integration checks.

### Unit-testable logic

- playlist discovery from `sample/`
- supported-file filtering
- default first-item selection
- selected-index changes on click intent
- play/pause state transitions at the application-state layer

### Manual integration checks

- the app launches on Linux
- the two sample videos appear in the sidebar
- the first item is selected without autoplay
- clicking another video switches the selection and loaded media
- the play/pause button changes playback state
- load or initialization failures show errors without crashing the app

### Explicit exclusions

The first version does not attempt automated verification of rendered video frames. That cost is not justified for this scope.

## Accepted Constraints

- Linux is the only platform the first version is designed for.
- Playlist discovery is limited to direct children of `sample/`.
- Supported file extensions are `.mp4`, `.mkv`, `.webm`, and `.mov`.
- Playlist ordering is alphabetical by display name.
- The player does not remember state between launches.
- Switching videos always returns the player to paused state.

## Success Criteria

The design is successful when an implementation can satisfy all of the following in a single GPUI window:

- the app lists the sample videos
- the first video is selected on startup but not playing
- clicking a different file switches the current video
- the play/pause control works on the selected video
- errors are visible and non-fatal for the overall application
