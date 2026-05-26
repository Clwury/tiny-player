# mpv-Style Read-Ahead Cache Design

## Context

The current `tiny` player is no longer a thin `libmpv` shell. Playback is owned by the in-repo FFmpeg backend under `src/player/backend/ffmpeg/`, while GPUI owns the video surface, timeline controls, subtitles, fullscreen behavior, and error/status presentation.

The current backend already contains two partial cache layers:

- HTTP byte cache: `src/player/backend/ffmpeg/avio/cache.rs`, exposed to FFmpeg through custom `AVIOContext` callbacks in `avio/callbacks.rs`.
- Demux packet cache: `src/player/backend/ffmpeg/playback_loop/demux_cache.rs`, consumed by `run_ffmpeg_playback` instead of calling `av_read_frame` directly in the decode loop.

The local mpv tree at `~/Projects/Application/mpv` implements a broader model:

- cache configuration lives in `demux/demux.c`, `demux/demux.h`, `options/options.c`, and `options/options.h`
- cache status is exposed through `demux_reader_state` and player properties in `player/command.c`
- automatic pause/resume on low cache is handled in `player/playloop.c`
- optional packet payload disk cache is implemented in `demux/cache.c`

This spec adapts the mpv behavior to the current Rust/FFmpeg architecture. It does not reintroduce `libmpv`.

The document is both a source analysis and an implementation contract. Sections that describe current status are snapshots of this worktree on 2026-05-25; sections that describe behavior are the target spec.

## Source Reference Map

Use these files as the implementation reference while building the feature.

### tiny

- `src/player/backend.rs`: backend command/control boundary.
- `src/player/backend/events.rs`: backend event payloads and public cache state/config types.
- `src/player/backend/ffmpeg.rs`: FFmpeg backend orchestration and backend-event adaptation.
- `src/player/backend/ffmpeg/worker.rs`: playback worker control flags, commands, interruption, and pause handling.
- `src/player/backend/ffmpeg/avio/cache.rs`: current HTTP byte cache and download state machine.
- `src/player/backend/ffmpeg/avio/callbacks.rs`: FFmpeg `AVIOContext` read/seek bridge.
- `src/player/backend/ffmpeg/format.rs`: FFmpeg format open/probe boundary.
- `src/player/backend/ffmpeg/playback_loop.rs`: main demux/decode/playback loop.
- `src/player/backend/ffmpeg/playback_loop/demux_cache.rs`: current packet prefetch/cache layer.
- `src/player/page/backend_events.rs`: GPUI playback state updates from backend events.
- `src/player/page/progress.rs` and `src/player/page/controls.rs`: timeline range calculation and rendering.

### mpv

- `demux/demux.h`: `demux_opts`, `demux_reader_state`, `demux_seek_range`, and public demux cache state surface.
- `demux/demux.c`: demux cache options/defaults, `demux_internal`, `demux_cached_range`, option resolution, packet range pruning, cached seek, and `demux_get_reader_state`.
- `demux/cache.c`: temporary append-only packet payload disk cache and unlink policy.
- `player/playloop.c`: `handle_update_cache`, cache pause, buffering percent, and cache update throttling.
- `player/command.c`: mpv properties built from `demux_reader_state`, including `demuxer-cache-state`, `paused-for-cache`, and `cache-buffering-state`.
- `options/options.c` and `options/options.h`: player-level cache pause and demuxer cache wait options/defaults.
- `DOCS/man/options.rst` and `DOCS/man/input.rst`: user-visible option/property semantics.

## mpv Source Findings

The behavior to mirror is concentrated in a small set of mpv internals:

- `demux/demux.c::demux_conf` defines cache defaults: `cache=auto`, `demuxer-max-bytes=150 MiB`, `demuxer-max-back-bytes=50 MiB`, `demuxer-donate-buffer=yes`, `demuxer-readahead-secs=1.0`, `cache-secs=1000h`, and `demuxer-seekable-cache=auto`.
- `demux/demux.h::demux_reader_state` is the canonical state model. It contains `eof`, `underrun`, `idle`, BOF/EOF cached flags, overall and per-stream timestamp windows, total/forward/file-cache byte counts, low-level and byte-level seek counters, input rate, and seekable ranges.
- `demux/demux.c::thread_work` keeps reading when an eager selected stream has no packet, a lazy stream is waiting, a stream is refreshing, or the forward cache is below the target duration. It stops on byte limits, EOF, or when hysteresis says not to resume yet.
- `demux/demux.c::prune_old_packets` separates forward bytes from old/backbuffer bytes. Backbuffer is limited by `demuxer-max-back-bytes`, can borrow unused forward budget when `demuxer-donate-buffer=yes`, and tries to prune fairly while preserving seekable keyframe boundaries.
- `demux/demux.c::find_cache_seek_range` accepts a cached seek when the target falls inside a seekable range, or outside the timestamp bounds only when the range is marked `is_bof` or `is_eof`.
- `demux/demux.c::execute_cache_seek` moves stream reader heads to cached packets and adjusts non-HR seeks to a safe video keyframe. If the target range is not the append range, mpv switches current range and queues a low-level resume seek near the range end so future appends continue from the right demuxer position.
- `demux/demux.c::demux_get_reader_state` derives public state from actual queues: `underrun` means an eager stream lacks a readable packet while the demuxer is not idle, `idle` means not reading or EOF, and global duration uses the selected stream type with the smallest known buffered duration.
- `demux/cache.c` implements append-only temporary packet payload storage. Packet metadata stays in memory; the file size is reported even if older data is no longer reachable. `demuxer-cache-unlink-files=immediate` unlinks the temp file right after creation while keeping the fd open.
- `player/playloop.c::handle_update_cache` owns cache pause semantics. Cache pause is enabled only for forward playback, waits for `cache-pause-wait`, enters after real demux plus output underrun after restart, allows initial pause only under `cache-pause-initial`, and caps displayed buffering below 100 while still paused.
- `player/command.c::mp_property_demuxer_cache_state` maps `demux_reader_state` to user-visible fields, including `debug-low-level-seeks` and `debug-byte-level-seeks`.

The implementation should copy these semantics, not the exact data structures.

## Current Working Tree Status

This spec is the target end state. The current working tree now contains most of the mpv-style cache architecture, but completion still depends on media-level validation under real HTTP playback, low-bandwidth, and long-session seek workloads.

Completed or substantially implemented:

- `PlaybackCacheConfig`, `PlaybackCacheState`, cache ranges, and cache events exist at the backend boundary.
- `BackendLoadRequest` carries cache config, and `BackendCommand::SetCacheConfig` plus `BackendControl::cache_state` exist.
- The FFmpeg backend emits `CacheStateChanged` and maps current HTTP/demux cache data into the unified state.
- Current HTTP and demux cache limits are partially config-driven.
- The frontend stores `PlaybackCacheState` and can render demux seekable ranges on the progress track.
- `FfmpegControl` now separates `user_paused` from `cache_paused`; effective pause is the union of both states.
- Cache pause events and buffering percent are emitted from demux read underrun/recovery paths, and post-start cache pause can now require an observed output underrun. Native audio underruns are detected from the audio callback; video starvation is marked when playback has no queued video frame and then blocks on demux cache data.
- HTTP probing now uses one per-load `CachedInputSource`, so fast/subtitle/full probe fallback attempts can reuse the same byte cache.
- The HTTP byte cache can retain multiple memory ranges across seek/restart boundaries and reports byte-level seek counts.
- The HTTP byte cache records a recent raw input rate and maps it into both `ByteCacheState` and `DemuxCacheState.raw_input_rate` for the unified cache state.
- HTTP retained byte ranges now track recent access and are pruned by least-recently-used order under the memory budget.
- HTTP byte cache `cached_bytes` now counts the union of reachable memory and disk byte ranges, so disk-backed ranges that overlap with memory are not double-counted in unified status.
- The demux packet cache now preserves old packet ranges across low-level seek misses, can seek back into archived ranges, enforces `demuxer_max_back_bytes` for archived backbuffer, and reports multiple seekable ranges through `PlaybackCacheState`.
- The demux packet cache now reports cached seek hits separately from low-level demuxer seeks and byte-level HTTP seeks.
- The demux packet cache now carries explicit BOF/EOF flags on active and archived ranges, and cached seek uses those flags for targets before the first cached packet or after the last cached packet.
- Cached seek into an archived demux range now immediately queues a low-level resume seek at that cached range's end and skips overlapping packets returned by the imprecise resume seek, matching mpv's append-resume behavior more closely than waiting for the cached range to be exhausted.
- The demux packet cache now records stream kinds for selected video/audio/subtitle streams and emits typed `StreamCacheState` entries instead of treating every stream as unknown.
- The top-level demux cache duration/cache end now follows mpv's selected-stream rule more closely: it uses the shortest forward window among selected streams, keeps reading when a selected audio/video stream has no forward packet, and ignores empty subtitle windows when video/audio forward cache exists.
- Demux seekable ranges and cached seek hits now intersect the video recovery/keyframe timeline with selected eager audio coverage, so cached seeks are rejected when audio is missing even if video packets exist.
- Demux underrun state now considers selected eager streams, so a missing forward packet for selected video/audio is reported even if other streams still have queued packets.
- The demux packet cache now applies `demuxer_hysteresis_secs` at the demux read-ahead layer: once the read-ahead target is reached, prefetch stays idle until forward cache duration falls to the hysteresis threshold.
- The demux packet cache now separates forward bytes from backward/cache-history bytes for budget decisions, and `demuxer_donate_buffer` lets unused forward byte budget extend the backbuffer limit.
- Archived demux ranges now prune the least-recently-used old range by selecting the earliest per-stream queue head by seekable start, pruning one safe packet run at that stream's boundary before falling back to whole-range removal, preserving later cached seek targets under backbuffer pressure.
- Demux packet disk cache creation now uses `PlaybackCacheConfig.cache_dir` and honors `CacheUnlinkPolicy::{Immediate, WhenDone, Never}`. `Immediate` unlinks the file after creation while keeping the open fd usable, matching mpv's default behavior.
- `BufferedChanged` compatibility events emitted by the demux packet cache are now derived from the same `PlaybackCacheState.demux.cache_end` that drives `CacheStateChanged`, rather than from a separate video-timeline reporter.
- `FfmpegBackend` no longer synthesizes demux cache duration, seekable ranges, BOF/EOF flags, or cached-seek counts from compatibility events or older state; those authoritative fields now come from `CacheStateChanged`.
- The progress bar now renders byte-cache ranges as a thinner underlay below demux seekable ranges when unified byte cache state includes content-length-backed ranges.
- The control overlay now has a compact cache status affordance backed by `PlaybackCacheState`, with input speed, demux duration, byte cache size, disk bytes, idle/running state, cache buffering percent, and seek counters.
- Seek commits now use `PlaybackCacheState.demux.seekable_ranges` to decide whether to keep the current frame and suppress immediate buffering for expected cached seeks, with `buffered_until` only as a fallback when no range list is available.
- The demux packet cache now stores read, append, and archived ranges in one `ranges` map keyed by `RangeId`; `read_range_id` and `append_range_id` are the authoritative cursors.
- `demuxer_cache_wait` now blocks playback restart until the demux cache reaches EOF or its configured prefetch/byte limit, and `demuxer_seekable_cache=Enabled` can force seekable backbuffer even when `cache=Disabled`.
- Demux per-stream cache windows now describe the current forward read/append window only; historical seek targets are exposed through `seekable_ranges`.
- The old HTTP DTOs have been removed from the backend event module; AVIO now builds unified `ByteCacheState` directly for byte-cache updates.
- Demux cache state is now recomputed and throttled after decoder packet reads as well as after appends, seeks, EOF, and pause transitions, so `reader_pts`, `cache_duration`, and `forward_bytes` advance while cached packets are consumed.
- Demux cache duration now remains `None` when cache end precedes reader position for either the aggregate state or per-stream state, rather than reporting a synthetic zero duration for invalid timestamp ordering.
- Seek and track-selection restarts clear stale output-underrun markers, so post-restart cache pause must observe a fresh output underrun before entering the mpv-style cache-pause gate.
- Seek/restart paths now preserve `user_paused` while clearing only cache pause, and the frontend play/pause affordance is driven by user pause state rather than effective pause, so cache pause alone does not look like a user pause.
- `DemuxCacheState` now carries mpv-like `seeking` state for queued/in-progress low-level demux seeks, including continuation seeks after archived cached-range hits.

Partially implemented but still below the target semantics:

- HTTP caching has multiple retained ranges, LRU pruning, raw input rate reporting, and a queued multi-worker side range downloader. Cached/probe reads and foreground playback misses for uncached offsets, including offsets before a trimmed active range, can queue independent side ranges without immediately restarting the active playback download. Side range creation now reports byte-cache activity promptly, and side workers are bounded to their own range-request budget instead of continuing to media EOF. When playback seeks outside the active byte range, the old active range is demoted to retained cache so it can still serve cache hits; when a foreground side range completes, the active worker is scheduled to continue from that range's end. EOF and last-side-range completion now publish byte-cache idle transitions through unified `CacheStateChanged` events.
- Cache pause is functional and now includes audio-output underrun detection plus a video-output starvation marker for demux waits with an empty video queue, but its enter/exit conditions still need manual low-bandwidth and seek-restart verification against mpv.
- Demux multi-range caching has explicit read/append/archived ranges, BOF/EOF flags, immediate append-resume seeks after archived cached seeks, per-stream queue-head pruning by seekable start, and sparse-stream pruned-watermark adjustment for cached seek ranges; it still needs broader media-level manual verification.
- Unified cache state exists and demux now reports multiple ranges/bytes/raw input rate/last demux timestamp/low-level seeks/stream kinds and read-side state changes, but some idle/underrun edge cases still need media-level verification.
- The frontend renders demux ranges, a byte-cache underlay, and the compact status affordance, and no longer keeps transitional `http_stream_*` timeline state.
- `HttpStreamBufferedChanged` and `HttpStreamCacheStatusChanged` have been removed; HTTP byte progress now flows through partial `CacheStateChanged` updates carrying `ByteCacheState` that `FfmpegBackend` merges before forwarding to the UI.

The remaining authoritative requirements are still:

- verify the unified demux range model under long playback and seek-heavy media sessions
- continue tightening cached seek hit/miss accounting now that cached-seek, low-level-seek, and byte-level-seek counters are exposed consistently
- validate the HTTP range cache with real media now that LRU budget management, raw input rate, side range workers, and disk byte cache accounting are implemented
- continue exposing mpv-like state fields from actual cache internals, not from compatibility-derived estimates
- polish cache pause semantics against manual low-bandwidth and seek-restart scenarios
- continue removing remaining compatibility adapters now that HTTP byte cache progress and demux buffered progress reach the UI through `PlaybackCacheState`

## Current Project Findings

### Backend

The original `FfmpegBackend` exposed only playback commands:

- load
- seek
- pause/resume
- stop
- track selection
- volume

The current working tree now has a cache config/state API backed by HTTP byte-cache and demux packet-cache internals. `CacheStateChanged` is the canonical event; `BufferedChanged` and `Buffering` remain compatibility/coarse signals only.

`BackendEventKind` now exposes cache changes through the unified cache events:

- `CacheStateChanged(PlaybackCacheState)`
- `PausedForCacheChanged(bool)`
- `CacheBufferingChanged(Option<u8>)`
- `BufferedChanged(Option<f64>)`
- `Buffering(bool)`

`BufferedChanged` and `Buffering` remain compatibility/coarse UI events. The unified event is the only cache boundary intended to carry seekable packet ranges, per-stream cache duration, underrun, raw input rate, low-level seek counts, byte-level seek counts, BOF/EOF cached state, and cache-pause buffering percentage.

`CacheStateChanged` should remain the canonical cache event. The backend no longer turns `BufferedChanged`, position updates, or older merged cache state into synthetic demux cache ranges or cached-seek counts; the remaining migration work is to reduce any remaining duplicated compatibility state and coarse buffering signals.

### HTTP Byte Cache

The HTTP cache currently supports:

- network-only activation for `http://` and `https://`
- blocking reads from FFmpeg's custom AVIO reader
- Range requests through `reqwest`
- one active in-memory download range
- multiple retained memory ranges across seek/restart/probe boundaries
- LRU pruning for retained memory ranges
- shared per-load cache reuse across FFmpeg fast/subtitle/full probe fallback attempts
- raw input rate calculation and byte-level seek accounting
- optional disk cache hooks that use `PlaybackCacheConfig.cache_dir` and `CacheUnlinkPolicy`, with debug env vars retained for byte budget overrides
- memory-retained and disk-backed HTTP byte ranges both prune least-recently-used ranges under their active budgets
- partial `CacheStateChanged` updates that carry unified `ByteCacheState`

Gaps relative to mpv-style behavior:

- not yet a fully independent arbitrary multi-download byte cache; the current implementation has one active network range plus queued side range workers for cached/probe/foreground misses before or after the active range, and foreground side ranges can schedule the active worker to continue from their end
- no complete runtime/user-visible cache option flow for HTTP-specific behavior
- disk byte cache ranges are integrated into unified status, use LRU pruning, and are counted without duplicating overlapping memory ranges, but byte-cache-specific budget tuning remains mostly debug/env-driven
- old HTTP-only events, frontend state, and event-layer HTTP DTOs have been removed; AVIO builds `ByteCacheState` directly
- byte ranges are a duration-aware UI underlay only when content length and duration are known; demux seekable ranges remain authoritative for cached time seeking

### Demux Packet Cache

The demux cache currently supports:

- a separate demux thread
- forward packet prefetching
- memory and read-ahead limits derived from `PlaybackCacheConfig`, with documented debug env overrides
- cached seek within the current active range and archived cached ranges
- HEVC seek preroll safety
- preservation of old packet ranges across low-level seek misses
- backbuffer pruning by `demuxer_max_back_bytes`
- multiple seekable range reporting through `PlaybackCacheState`
- selected video/audio/subtitle stream kind reporting
- cache pause events and buffering percentage from demux read underrun/recovery paths
- optional demux packet disk payload cache via `PlaybackCacheConfig.disk_cache`, with `TINY_DEMUX_PACKET_CACHE_ON_DISK` retained as a debug override
- `BufferedChanged` compatibility events derived from `PlaybackCacheState.demux.cache_end`

Remaining risks relative to mpv-style behavior:

- cache config resolution now covers `cache`, `cache-secs=0`, `demuxer-readahead-secs`, `demuxer-max-bytes=0`, and mode-specific cache activation; remaining risk is parity validation on real media
- `demuxer-max-bytes`, `demuxer-max-back-bytes`, `demuxer-hysteresis-secs`, and `demuxer-donate-buffer` now participate in demux cache decisions, and archived ranges can shrink by mpv-like per-stream queue-head selection at recovery/keyframe or timestamp boundaries with sparse-stream pruned-watermark adjustment; remaining pruning risk is media-level validation
- the range model now has explicit read, append, and archived ranges in one range map, and archived cached seeks queue an immediate append-resume seek
- BOF/EOF cached flags are explicit range properties; per-stream state now tracks the forward read/append window rather than archived history
- demux EOF now maps to idle even when effective EOF is detected through a detached append range after an archived cached seek
- old-range pruning now has LRU range selection, keyframe/recovery-point packet-run trimming, and sparse-stream pruned-watermark adjustment; remaining risk is real-media validation
- underrun/idle/raw-rate fields are reported from cache internals, with remaining risk focused on real-media edge-case validation
- `ts_last` is reported from the last demuxed packet timestamp and clears on low-level seek requests, matching mpv's `debug-ts-last` reset semantics.
- cache pause is implemented with audio-output-underrun gating and a video-output starvation marker, but still needs manual low-bandwidth and seek-restart verification

### Frontend

`PlaybackPage` stores:

- `buffered_until`
- `cache_state`
- `paused_for_cache`
- `cache_buffering_percent`
- `buffering`
- `pending_seek_position`

The progress UI can render:

- played fraction
- seekable demux cache ranges when available
- one continuous buffered fraction from `buffered_until` as fallback
- byte-cache ranges as a thinner underlay when content length and duration are known
- a compact cache status popover from unified cache state
- drag-time highlighting for whether a seek target can be satisfied from cache

The main timeline remains intentionally focused on played and cached ranges. Secondary diagnostics such as idle/running state, cache pause percentage, input speed, disk usage, and seek counters are exposed through the compact cache status popover. Committed seeks use seekable ranges to decide whether to keep the current frame, and progress dragging colors cache-miss targets differently from cached targets.

## mpv Behavior To Mirror

This project should mirror mpv's behavior at the semantic level, not its C implementation.

### Options

mpv's cache-relevant options include:

- `cache=<yes|no|auto>`
- `cache-secs=<seconds>`
- `cache-on-disk=<yes|no>`
- `demuxer-readahead-secs=<seconds>`
- `demuxer-hysteresis-secs=<seconds>`
- `demuxer-max-bytes=<bytes>`
- `demuxer-max-back-bytes=<bytes>`
- `demuxer-donate-buffer=<yes|no>`
- `demuxer-seekable-cache=<yes|no|auto>`
- `force-seekable=<yes|no>`
- `demuxer-thread=<yes|no>`
- `demuxer-cache-wait=<yes|no>`
- `cache-pause=<yes|no>`
- `cache-pause-initial=<yes|no>`
- `cache-pause-wait=<seconds>`
- `demuxer-cache-dir=<path>`
- `demuxer-cache-unlink-files=<immediate|whendone|no>`

`tiny` should expose the same concepts through Rust names. The initial implementation can keep persistence out of scope, but the runtime types should not be env-only.

### State

mpv's `demux_reader_state` is the target model:

- `eof`
- `underrun`
- `idle`
- `bof_cached`
- `eof_cached`
- `ts_info.reader`
- `ts_info.end`
- `ts_info.duration`
- `ts_per_stream`
- `total_bytes`
- `fw_bytes`
- `file_cache_bytes`
- `seeking`
- tiny extension: `cached_seeks`
- `low_level_seeks`
- `byte_level_seeks`
- `ts_last`
- `bytes_per_second`
- `seek_ranges`

The player properties built from that state are:

- `cache-speed`
- `demuxer-cache-duration`
- `demuxer-cache-time`
- `demuxer-cache-idle`
- `demuxer-cache-state`
- `cache-buffering-state`
- `paused-for-cache`

`tiny` should expose an equivalent single cache state event to the UI instead of spreading partial state across HTTP and demux events.

### Read-Ahead

mpv reads ahead while any selected eager stream needs packets or while the forward cache has less than the target duration. It stops prefetching when either a time target or byte target is reached. If hysteresis is configured, it stays idle until the remaining buffered duration drops below the resume threshold.

In `tiny`, the demux packet cache is authoritative for time read-ahead. The HTTP byte cache is an optimization layer below FFmpeg's AVIO callbacks; its byte ranges can help explain network progress but must not be used as proof that a time seek can be satisfied without demux packets.

### Seekable Cache

mpv keeps cached packet ranges and lets seeks land inside those ranges without a low-level demuxer seek. Seeking outside the cache creates a fresh range and may keep older ranges as backbuffer, constrained by memory/disk budgets.

For `tiny`, a cached seek is successful only if the demux packet cache can reposition to a safe packet for all selected streams required to restart decoding. Byte-cache hits alone are not cached seeks.

### Cache Pause

mpv separates user pause from cache pause:

- effective pause is `user pause || paused_for_cache`
- cache pause enters buffering only on real demux/output underrun, except for initial cache pause
- playback resumes when cached duration reaches `cache-pause-wait`, or earlier if the demuxer becomes idle/EOF before reaching the threshold
- UI-visible buffering percentage is `cache_duration / cache_pause_wait`, capped below 100 while still buffering

The current tree already splits `FfmpegControl` into `user_paused` and `cache_paused`; post-start cache pause now observes native audio-output underruns and marks video-output starvation when playback has no queued video frame and then waits for demux data. The remaining work is manual low-bandwidth and seek-restart verification against mpv's demux/output underrun semantics.

## Required State Invariants

The unified cache state emitted by `BackendEventKind::CacheStateChanged` must obey these invariants:

- `DemuxCacheState.seekable_ranges` describes time ranges that can be used for cached demux seeks. It must be based on packet ranges and keyframe/recovery-point safety, not HTTP byte ranges.
- `DemuxCacheState.cache_duration` is `cache_end - reader_pts` only when both values are known and ordered. Unknown or invalid timestamps should be `None`, not `0`.
- `DemuxCacheState.forward_bytes` counts bytes from the current reader position to the current demux append position. `total_bytes` includes retained cached ranges.
- `DemuxCacheState.file_cache_bytes` reports the packet payload disk cache file size when disk cache is enabled, including unreachable appended data, matching mpv's semantics.
- `bof_cached` and `eof_cached` are explicit range properties. They must not be inferred only from `start <= 0` or duration equality.
- `cached_seeks` increments only when no low-level FFmpeg seek is needed for the user's target. `low_level_seeks` increments when `FormatContext::seek_stream` or an equivalent demuxer reposition is queued. `byte_level_seeks` increments when AVIO moves outside the current contiguous byte range.
- `paused_for_cache` is separate from user pause. Clearing cache pause must not clear user pause.
- HTTP `ByteCacheState.ranges` can be shown as a lower-confidence byte underlay only when `content_length` is known. It must not replace demux `seekable_ranges`.
- State events should be throttled during continuous prefetch, but seek, EOF, underrun, cache-pause enter/exit, and range creation/pruning must be reported promptly.

## Goals

- Implement mpv-style pre-read caching on top of the current FFmpeg backend.
- Preserve current playback, track switching, subtitles, Dolby Vision handling, Vulkan/CPU frame paths, and GPUI presentation.
- Make cache behavior configurable without relying only on compile-time constants or env vars.
- Expose one coherent cache state model to the frontend.
- Render useful cache information in the timeline without turning the player into a diagnostics dashboard.
- Make in-cache seeks visible and fast, and avoid low-level seeks when cached data is sufficient.
- Keep HTTP byte caching and demux packet caching separate internally, but unified at the backend event/API boundary.

## Non-Goals

- Replacing the FFmpeg backend with `libmpv`.
- Implementing mpv's backward playback.
- Implementing playlist prefetch in this spec.
- Persisting cache settings in app storage in the first implementation pass.
- Adding a full settings page unless a separate settings design asks for it.
- Making cache files reusable across media sessions. mpv treats demux cache files as temporary, and `tiny` should do the same.

## Public Backend API

### Configuration Types

Add a backend-level cache config near `src/player/backend/events.rs` or a new `src/player/backend/cache.rs`.

```rust
#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackCacheConfig {
    pub mode: PlaybackCacheMode,
    pub seekable_cache: PlaybackSeekableCacheMode,
    pub disk_cache: bool,
    pub disk_cache_max_bytes: u64,
    pub cache_secs: f64,
    pub demuxer_readahead_secs: f64,
    pub demuxer_hysteresis_secs: f64,
    pub demuxer_max_bytes: u64,
    pub demuxer_max_back_bytes: u64,
    pub demuxer_donate_buffer: bool,
    pub http_cache_max_bytes: u64,
    pub http_cache_chunk_bytes: u64,
    pub http_cache_range_request_bytes: u64,
    pub cache_pause: bool,
    pub cache_pause_initial: bool,
    pub cache_pause_wait: f64,
    pub demuxer_cache_wait: bool,
    pub cache_dir: Option<PathBuf>,
    pub unlink_files: CacheUnlinkPolicy,
}
```

Defaults should map to mpv where practical:

- `mode: Auto`
- `seekable_cache: Auto`
- `disk_cache: false`
- `disk_cache_max_bytes: 4 GiB`
- `demuxer_readahead_secs: 1.0`
- `demuxer_hysteresis_secs: 0.0`
- `demuxer_max_bytes: 150 MiB`
- `demuxer_max_back_bytes: 50 MiB`
- `demuxer_donate_buffer: true`
- `http_cache_max_bytes: 500 MiB`
- `http_cache_chunk_bytes: 1 MiB`
- `http_cache_range_request_bytes: 32 MiB`
- `cache_secs: very large`, but practically capped by `demuxer_max_bytes`
- `cache_pause: true`
- `cache_pause_initial: false`
- `cache_pause_wait: 1.0`
- `demuxer_cache_wait: false`
- `unlink_files: Immediate`

For the initial UI, a conservative app default can override mpv's very high `cache_secs` with the current `120 s` behavior if needed, but this must be expressed in config rather than hidden constants.

### State Types

Use the unified state directly for byte cache status:

```rust
#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackCacheState {
    pub demux: DemuxCacheState,
    pub byte: Option<ByteCacheState>,
    pub paused_for_cache: bool,
    pub buffering_percent: Option<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DemuxCacheState {
    pub cache_end: Option<f64>,
    pub reader_pts: Option<f64>,
    pub cache_duration: Option<f64>,
    pub eof: bool,
    pub underrun: bool,
    pub idle: bool,
    pub seeking: bool,
    pub bof_cached: bool,
    pub eof_cached: bool,
    pub total_bytes: u64,
    pub forward_bytes: u64,
    pub file_cache_bytes: Option<u64>,
    pub raw_input_rate: Option<u64>,
    pub ts_last: Option<f64>,
    pub cached_seeks: u64,
    pub low_level_seeks: u64,
    pub byte_level_seeks: u64,
    pub seekable_ranges: Vec<PlaybackCacheTimeRange>,
    pub streams: Vec<StreamCacheState>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ByteCacheState {
    pub ranges: Vec<PlaybackCacheByteRange>,
    pub reader_fraction: Option<f64>,
    pub download_fraction: Option<f64>,
    pub cached_bytes: u64,
    pub content_length: Option<u64>,
    pub disk_cache_enabled: bool,
    pub idle: bool,
    pub raw_input_rate: Option<u64>,
    pub byte_level_seeks: u64,
}
```

The old `HttpStreamCacheStatus` compatibility adapter has been removed. AVIO reports byte-cache increments as `ByteCacheState` inside a partial `PlaybackCacheState`, and `FfmpegBackend` merges that with the last demux state.

### Events

Add:

- `BackendEventKind::CacheStateChanged(PlaybackCacheState)`
- `BackendEventKind::PausedForCacheChanged(bool)`
- `BackendEventKind::CacheBufferingChanged(Option<u8>)`

The frontend and backend event surface are already migrated away from the old HTTP-only cache events.

Keep `BufferedChanged` only if it remains useful as a simple progress fallback. It must be derived from `PlaybackCacheState.demux.cache_end`, not maintained independently.

### Commands

Add:

- `BackendCommand::SetCacheConfig(PlaybackCacheConfig)`
- `BackendControl::cache_state(&self) -> Option<PlaybackCacheState>`

`SetCacheConfig` should apply live where safe:

- read-ahead seconds, hysteresis, byte limits, pause settings: live
- disk cache creation: can be enabled live for future packets
- disk cache disabling: stop writing new packets but keep the old temp file until media closes
- cache dir and unlink policy: apply on next cache file creation

## Backend Architecture

### 1. Split User Pause From Cache Pause

Change `FfmpegControl` from one `paused` flag to:

- `user_paused`
- `cache_paused`
- `effective_paused()`

Playback loops should wait while `effective_paused()` is true. User pause commands modify only `user_paused`. Cache pause logic modifies only `cache_paused`.

Event semantics:

- `Pause(bool)` continues to mean user-visible effective pause for existing UI controls.
- `PausedForCacheChanged(bool)` tells UI why playback is effectively paused.
- If the user pauses while cache pause is active, cache pause may later clear without auto-resuming playback because `user_paused` remains true.

### 2. Promote Cache Config Into Runtime State

Move these hard-coded constants into config:

- `DEMUX_PACKET_CACHE_MEMORY_BYTES`
- `DEMUX_PACKET_CACHE_READAHEAD_NSECS`
- `DEMUX_PACKET_CACHE_DEFAULT_DISK_BYTES`
- `HTTP_RING_CACHE_CAPACITY`
- `HTTP_CACHE_RANGE_REQUEST_BYTES`
- `HTTP_CACHE_DEFAULT_READAHEAD_SECONDS`
- `HTTP_CACHE_DEFAULT_HYSTERESIS_SECONDS`

Environment variables can remain debug overrides, but the source of truth should be `PlaybackCacheConfig`.

`FfmpegPlaybackInput` should carry the resolved config, and `FfmpegWorker` should hold an atomic or mutex-protected live config snapshot for runtime changes.

### 3. Share HTTP Cache Across Probe Fallback

`open_playback_input_with_fallback` can currently create and drop separate HTTP caches during fast/subtitle/full probe retries. The spec requires a per-load `CachedInputSource`:

- construct one `HttpRingCache` for the load request
- pass cloned handles into every `FormatContext::open` attempt
- keep the same cache when falling back from fast probe to deeper probe
- clear only on media unload or backend stop

This avoids re-downloading initial media bytes and preserves early byte cache state.

Implementation note: this is already substantially implemented in the current worktree. Keep it as an invariant when refactoring `format.rs` and AVIO setup.

### 4. Upgrade HTTP Byte Cache To Multi-Range

Replace the single active ring plus one retained tail range with an explicit range set:

- active download range
- cached ranges in memory
- optional disk-backed byte ranges
- LRU trimming by total byte budget
- tail metadata probes stored as normal ranges, not a special retained slot

Required behavior:

- any read satisfied by an existing memory/disk range returns immediately
- a miss queues/restarts a download at the requested byte offset
- byte-level seek count increments when FFmpeg's AVIO seek moves outside the current contiguous range
- raw input rate is calculated over a sliding one-second window from network reads
- disk cache writes are best-effort; failures disable future disk writes without failing playback
- all ranges are reported in `ByteCacheState`

The old single-progress HTTP buffer DTO has been removed; UI progress must use `ByteCacheState.ranges`.

HTTP byte-cache completion criteria:

- reads inside any retained memory or disk range do not restart the network download
- tail metadata probes become normal ranges and are marked separately only for diagnostics
- LRU pruning accounts for all retained ranges, not only inactive tails
- disk cache uses `PlaybackCacheConfig.cache_dir` and `unlink_files` rather than only env vars
- a failed disk write disables future disk writes for the session without changing playback behavior

### 5. Rework Demux Cache Around Cached Ranges

Introduce a range model similar to mpv's `demux_cached_range`.

```rust
struct DemuxCachedRange {
    streams: BTreeMap<c_int, VecDeque<u64>>,
    global_order: VecDeque<u64>,
    seek_start_nsecs: Option<u64>,
    seek_end_nsecs: Option<u64>,
    is_bof: bool,
    is_eof: bool,
    last_used_generation: u64,
}
```

`DemuxPacketCacheState` should own:

- all cached packets by id
- multiple cached ranges
- current append/read range
- forward byte count
- total byte count
- disk packet payload cache
- reader timeline position
- demuxer timeline position
- low-level seek count
- byte-level seek count forwarded from HTTP cache when available

The current active-plus-archived implementation is an acceptable intermediate state, but the final model should make append range, read range, and retained ranges explicit. This is required to match mpv's behavior when reading from an old cached range while the demuxer must later resume appending near that range's end.

### 6. Forward Read-Ahead Algorithm

The demux thread should keep reading if:

- a selected stream has no packet ready for the decoder
- a lazy stream was forced to read to a target
- the forward buffered duration is below the configured target

The thread should stop reading if:

- forward bytes reach `demuxer_max_bytes`
- configured read-ahead duration is reached
- EOF is reached
- shutdown or pending seek interrupts it

When a limit is reached:

- set `idle = true`
- if `demuxer_hysteresis_secs > 0`, do not resume until remaining forward cache drops to or below that hysteresis threshold
- emit `CacheStateChanged`

When cache mode is active, effective read-ahead target is:

- `max(demuxer_readahead_secs, cache_secs)`, still capped by bytes

When cache mode is inactive:

- seekable cache is disabled unless `demuxer-seekable-cache=yes`
- max back bytes becomes zero
- `cache_pause` is ignored

Hysteresis must be applied to demux read-ahead, not just HTTP byte prefetch. When `demuxer_hysteresis_secs > 0`, the demux thread should report `idle = true` after reaching the target and stay idle until the remaining forward duration is at or below the hysteresis threshold.

### 7. Backbuffer And Pruning

Preserve packets behind the reader when seekable cache is enabled.

Memory budget:

- forward packets are constrained by `demuxer_max_bytes`
- past packets are constrained by `demuxer_max_back_bytes`
- if `demuxer_donate_buffer` is true, unused forward budget can be donated to backbuffer

Pruning rules:

- prefer pruning least-recently-used old ranges
- preserve keyframe boundaries for seekable video ranges
- never prune packets at or after the current reader cursor
- if the cache is not seekable, prune aggressively and keep no backbuffer
- update `seekable_ranges`, `bof_cached`, and `eof_cached` after pruning

### 8. Cached Seek

On seek:

1. Convert target seconds to timeline nanoseconds.
2. If seekable cache is enabled, find a cached range where:
   - target is between `seek_start` and `seek_end`, or
   - target is before start and `is_bof`, or
   - target is after end and `is_eof`.
3. If found, choose the nearest safe video seek packet:
   - keyframe/recovery point at or before target
   - HEVC uses the existing preroll requirement
   - if no safe packet exists, treat as miss
4. Move read cursors and decoder timeline to the cached packet.
5. Do not call `FormatContext::seek_stream`.
6. Increment `cached_seeks`.
7. Emit `CacheStateChanged` and a `BufferedChanged` compatibility event.

On miss:

1. Clear decoder state as today.
2. Create a fresh current cached range.
3. Queue a low-level seek.
4. Increment `low_level_seeks`.
5. Continue prefetch from the new demuxer position.

The frontend should be able to ask whether a target is inside `seekable_ranges` before showing a buffering overlay.

For archived ranges, `tiny` must preserve enough decode preroll to make an in-cache seek visually correct. For HEVC, keep the existing recovery/preroll logic; for other video codecs, keyframes or recovery points are the minimum safe target. If no safe target exists before the requested position inside the range, report a cache miss.

### 9. Disk Packet Cache

Implement mpv-like temporary packet payload disk cache:

- packet payloads are appended to a temp file
- packet metadata stays in memory
- file space is not reused
- cache file is deleted when playback closes unless unlink policy says otherwise
- disk writes are optional and best-effort
- `file_cache_bytes` reports file size, not just currently reachable ranges

Use the existing `DemuxPacketDiskCache` as the starting point, but make it config-driven instead of env-only.

### 10. Cache Pause

Add cache pause state to the playback loop.

Inputs:

- `cache_pause`
- `cache_pause_initial`
- `cache_pause_wait`
- demux cache `underrun`
- forward cache duration
- EOF/idle state

Rules:

- if user paused, do not auto-resume when cache recovers
- if `cache_pause_initial` is true, hold effective playback before first start until cache duration reaches `cache_pause_wait`, EOF, or idle
- after playback has started, enter cache pause only if the decoder/output actually waited for missing demux data
- while cache paused, emit `CacheBufferingChanged(Some(percent))`
- leave cache pause when cache duration reaches wait target, or when demux is idle/EOF and no more prefetch is possible
- on leave, emit `CacheBufferingChanged(None)` and `PausedForCacheChanged(false)`

The existing `Buffering(bool)` can remain as a coarse UI signal, but the frontend should use `PausedForCacheChanged` and `CacheBufferingChanged` for the cache-specific overlay.

mpv-aligned entry condition after restart:

- `cache_pause` is enabled
- playback direction is forward
- demux state is not idle
- demux cache duration is below `cache_pause_wait`
- a demux underrun has occurred
- an audio or video output underrun has occurred, or this backend has no output-underrun detector and must conservatively treat the demux wait as output starvation

Initial cache pause skips the output-underrun requirement but only applies before playback restart when `cache_pause_initial` is enabled.

## Frontend Design

### State

Extend `PlaybackTimelineState`:

```rust
pub(super) cache_state: Option<PlaybackCacheState>,
pub(super) paused_for_cache: bool,
pub(super) cache_buffering_percent: Option<u8>,
```

The old `http_stream_buffered_range`, `http_stream_cache_status`, and `http_stream_buffer_poll_active` fields have been removed. Future frontend cache behavior must be derived from `PlaybackCacheState`.

### Event Handling

`PlaybackPage::apply_backend_event` should:

- store `CacheStateChanged`
- derive `buffered_until` from `cache_state.demux.cache_end` for compatibility
- store `paused_for_cache`
- store `cache_buffering_percent`
- keep `Buffering(bool)` only as generic loading/seeking state

When playback is paused but cache is active, continue scheduling backend polls or notification frames until the cache reports idle/EOF. This replaces the current HTTP-only paused poll gate.

### Timeline Rendering

The progress bar should render three layers:

1. full track background
2. seekable demux time ranges
3. played progress

Optional fourth layer:

- byte cache ranges as a thinner, lower-opacity underlay if `content_length` and duration are both known

Rules:

- seekable demux ranges are authoritative for in-cache time seeking
- `buffered_until` is a simple fallback only when no range list is available
- ranges must clamp to `[0, duration]`
- overlapping ranges should be merged before rendering
- if target seek position is inside a seekable range, do not show a cache buffering overlay immediately
- if target seek position is outside all seekable ranges, show normal seek/loading status until backend restarts playback

### Cache Status Presentation

Add a small cache status affordance to the existing control overlay:

- icon-only button, no explanatory in-app text
- tooltip or compact popover can show:
  - input speed
  - demux cache duration
  - cache idle/running
  - disk cache bytes
  - low-level seeks
  - byte-level seeks

This must remain secondary. The core video controls should stay visually dominant.

### Buffering Overlay

When `paused_for_cache` is true:

- show a buffering state over the video
- if `cache_buffering_percent` is present, show the percent
- keep play/pause icon semantics clear: a user pause is different from cache pause

If user pause and cache pause are both active:

- the pause button should resume only user pause
- cache pause may still keep playback held until recovered
- status copy should indicate buffering, not user pause

### Seeking UX

During progress drag:

- highlight whether the target is inside a seekable demux range
- commit seek normally
- keep the existing frame if the seek is cached
- show loading/buffering only on expected cache miss or backend-confirmed buffering

The implementation can defer thumbnail preview; this spec does not require it.

## Traceability Matrix

| mpv concept | mpv source | tiny target |
| --- | --- | --- |
| Cache options/defaults | `demux/demux.c::demux_conf`, `options/options.c` | `PlaybackCacheConfig`, load request config, live `SetCacheConfig` |
| Packet cache state | `demux_reader_state` | `PlaybackCacheState.demux` |
| Byte input rate/seeks | `demux_get_reader_state`, stream layer controls | `ByteCacheState.raw_input_rate`, `byte_level_seeks`, merged into demux state |
| Read-ahead loop | `thread_work` | `run_demux_packet_cache` and `DemuxPacketCacheState::should_pause_demux` replacement |
| Seekable ranges | `demux_cached_range`, `update_seek_ranges` | explicit demux range model and `seekable_ranges` |
| Cached seek | `find_cache_seek_range`, `execute_cache_seek` | `DemuxPacketCache::seek`, no FFmpeg seek on hit |
| Backbuffer pruning | `prune_old_packets` | LRU/range pruning with keyframe-safe packet removal |
| Disk packet cache | `demux/cache.c` | config-driven `DemuxPacketDiskCache` |
| Cache pause | `handle_update_cache` | `FfmpegControl::cache_paused`, cache pause events, buffering percent |
| Frontend properties | `player/command.c`, `DOCS/man/input.rst` | GPUI timeline ranges, cache popover, buffering overlay |

## Completion Checklist

- All cache-visible frontend state is derived from `PlaybackCacheState`, `PausedForCacheChanged`, and `CacheBufferingChanged`.
- Legacy HTTP-only status DTOs are removed; no UI code depends on HTTP-only events or fields.
- Demux seekable ranges survive low-level seek misses, are pruned by budget, and are sorted/merged for UI rendering.
- Cached seek tests prove that `cached_seeks` increments without `low_level_seeks` for targets inside current and retained ranges.
- Miss seek tests prove that a fresh append range is created and `low_level_seeks` increments.
- BOF/EOF range tests cover positive first packet timestamps, seeks before first packet, seeks after last packet, and archived ranges.
- Cache pause tests cover initial wait, post-restart underrun wait, user pause during cache pause, recovery on duration threshold, recovery on EOF/idle, and percent capping below 100 while paused.
- HTTP tests cover retained range hit, LRU pruning, tail metadata probe reuse, byte-level seek counting, raw input rate, disk readback, and shared probe fallback.
- Frontend tests cover demux range fraction clamping/merging, byte underlay clamping/merging, cached seek target detection, and cache status segment formatting.
- Manual verification covers Emby HTTP playback, low bandwidth, backward cached seek, outside-cache seek, long playback memory stability, and disk cache unlink policy.

## Data Flow

### Load

1. `PlaybackPage` creates `BackendLoadRequest` with resolved `PlaybackCacheConfig`.
2. `FfmpegBackend` spawns one worker and one per-load cached input source.
3. FFmpeg probing uses the shared HTTP byte cache.
4. `DemuxPacketCache` starts with an empty current range.
5. Backend emits initial `CacheStateChanged`.
6. If `demuxer_cache_wait` or `cache_pause_initial` applies, backend prefetches before playback restart is emitted.

### Playback

1. Demux thread reads packets into the current range.
2. Decode loop reads packets from `DemuxPacketCache`.
3. Cache state is recomputed after packet append, read, seek, EOF, underrun, pause/resume, and byte cache progress.
4. Backend throttles `CacheStateChanged` to avoid excessive UI churn.
5. UI updates timeline ranges and cache status.

### Cached Seek

1. UI commits seek target.
2. Backend asks demux cache for cached seek.
3. On hit, demux cache moves read cursor and emits state.
4. Decode loop flushes decoders and resumes from cached packet.
5. No HTTP range restart or low-level demuxer seek occurs unless FFmpeg needs missing byte data.

### Cache Miss Seek

1. Demux cache creates a fresh current range.
2. `FormatContext::seek_stream` performs low-level seek.
3. HTTP cache may restart byte download at the requested offset.
4. Backend reports `seeking`, cache state, and generic buffering/loading as needed.

### Cache Pause

1. Decode loop detects that demux cache cannot satisfy a packet read promptly.
2. If cache pause is enabled and conditions match, backend sets `cache_paused = true`.
3. UI receives `PausedForCacheChanged(true)` and `CacheBufferingChanged(Some(percent))`.
4. Demux thread keeps prefetching.
5. Once recovered, backend clears `cache_paused` and resumes only if user pause is false.

## Implementation Phases

### Phase 1: Types And Event Boundary

Status: substantially implemented.

- Add `PlaybackCacheConfig`, `PlaybackCacheState`, and range/state structs.
- Add backend events and command surface.
- Keep current cache internals but emit the new state from existing values.
- Migrate `PlaybackPage` to consume `CacheStateChanged`.

### Phase 2: Runtime Config

Status: partially implemented.

- Replace demux and HTTP cache constants with resolved config.
- Add validation and defaults.
- Keep env vars as overrides for local debugging only.
- Thread config through load, worker, HTTP cache, and demux cache.
- HTTP byte cache memory and chunk budgets now come from `PlaybackCacheConfig`, with `TINY_HTTP_CACHE_*` retained only as debug overrides.
- HTTP Range request size and HTTP/demux disk-cache byte budgets now come from `PlaybackCacheConfig`, with env vars retained only as debug overrides.
- `demuxer_hysteresis_secs` now participates in demux read-ahead pause/resume, not only HTTP byte prefetch.
- `demuxer_max_bytes`, `demuxer_max_back_bytes`, and `demuxer_donate_buffer` now separate forward cache limits from backward cache limits in the demux packet cache.
- `cache=auto` is now resolved from input cacheability before configuring the demux packet cache: HTTP/HTTPS input gets cache-active behavior, while local input keeps normal `demuxer_readahead_secs` behavior unless cache or seekable cache is explicitly forced.
- `cache_secs=0` is accepted like mpv and lets cache-active read-ahead fall back to `demuxer_readahead_secs` instead of silently restoring the huge default.
- `demuxer_max_bytes=0` is preserved like mpv, and the demux thread can still read when a selected eager stream has no forward packet instead of treating the byte limit as an absolute dead stop.

### Phase 3: Demux Cache Ranges

Status: substantially implemented; read, append, and archived ranges now share one `RangeId` map, with remaining work focused on media-level verification.

- Introduce `DemuxCachedRange`.
- Preserve multiple cached ranges across low-level seeks.
- Add seekable range calculation and cached seek hit/miss logic.
- Add backbuffer budget, donation, and keyframe-aware pruning.
- Backbuffer budget and donation are implemented, and archived ranges prune the LRU old range by choosing the earliest per-stream queue head before whole-range removal. Sparse-stream pruned watermarks keep cached seek ranges from covering pruned subtitle windows; remaining work is real-media parity validation against mpv's `prune_old_packets`.
- Add BOF/EOF flags and per-stream cache duration.
- Emit throttled cache state after packet reads, so reader position and forward cache accounting do not depend only on future appends.

### Phase 4: Cache Pause

Status: partially implemented; audio-output underrun gating, video-output starvation marking, cache-pause wait-target recovery, live wait-target/disable transitions, and EOF/idle early recovery are covered by automated tests, while manual low-bandwidth and seek-restart verification remain.

- Split user pause and cache pause.
- Add underrun detection to demux read waits.
- Implement `cache_pause`, `cache_pause_initial`, and `cache_pause_wait`.
- Clear cache pause when demux reaches EOF/idle before `cache_pause_wait`.
- Emit cache buffering percentage.

### Phase 5: HTTP Multi-Range And Shared Probe Cache

Status: partially implemented; shared probe cache, retained ranges, raw input rate, byte-level seek counts, memory/disk LRU pruning, queued side range workers for cached/probe/foreground misses, prompt side-range activity reporting, bounded side range downloads, active continuation after foreground side ranges, byte-cache idle reporting on EOF/side-range completion, direct `ByteCacheState` reporting, and `cache=Disabled` bypass of the HTTP byte cache exist, but broader media-level verification and fully user-visible byte-cache option flow remain.

- Share one HTTP cache across probe fallback.
- Replace retained tail range with multi-range byte cache.
- Add raw input rate, byte-level seek count, and disk byte cache config.
- Keep byte-cache updates as partial unified cache state and merge them with demux state in `FfmpegBackend`.

### Phase 6: Frontend Timeline And Cache UI

Status: substantially implemented for timeline ranges, byte-cache underlay, compact cache status, paused polling while cache work is active, removal of old HTTP-only UI/event paths, committed cached-seek frame retention, and drag-time cached-seek highlighting.

- Render seekable ranges on the progress track.
- Render byte cache ranges as optional underlay.
- Add compact cache status affordance.
- Update seek behavior to use seekable ranges for cache-hit expectations.
- Remove obsolete `http_stream_*` timeline fields.

### Phase 7: Cleanup

Status: started.

- Remove deprecated events and compatibility adapters.
- Move cache docs into code comments where useful.
- Add a short developer note with config defaults and debugging env vars.
- Demux packet disk cache unlink policy is now config-driven, and debug env overrides are documented below.

## Developer Debug Overrides

`PlaybackCacheConfig` is the source of truth for runtime behavior. The following environment variables are local debugging overrides only and should not be surfaced as user settings before they are promoted into typed config:

- `TINY_HTTP_CACHE_MEMORY_BYTES`: overrides the in-memory HTTP byte-cache budget after `PlaybackCacheConfig::normalized`.
- `TINY_HTTP_CACHE_CHUNK_BYTES`: overrides HTTP worker read chunk size, clamped to `64 KiB..16 MiB`.
- `TINY_HTTP_CACHE_RANGE_REQUEST_BYTES`: overrides HTTP Range request size, clamped to `64 KiB..128 MiB` and at least the configured chunk size.
- `TINY_HTTP_CACHE_READAHEAD_SECS`: overrides HTTP byte-cache prefetch target duration.
- `TINY_HTTP_CACHE_HYSTERESIS_SECS`: overrides HTTP byte-cache resume hysteresis.
- `TINY_HTTP_CACHE_MAX_BYTES`: overrides HTTP maximum read-ahead bytes.
- `TINY_HTTP_CACHE_DISK_BYTES`: overrides `PlaybackCacheConfig.disk_cache_max_bytes` for the HTTP disk byte cache when disk cache is enabled.
- `TINY_HTTP_CACHE_DIR`: overrides HTTP disk byte-cache directory when `PlaybackCacheConfig.cache_dir` is unset.
- `TINY_DEMUX_PACKET_CACHE_ON_DISK`: enables demux packet payload disk cache even when runtime config disables disk cache.
- `TINY_DEMUX_PACKET_CACHE_BYTES`: overrides `PlaybackCacheConfig.disk_cache_max_bytes` for demux packet payload disk cache.
- `TINY_DEMUX_PACKET_CACHE_DIR`: overrides demux packet payload disk-cache directory when `PlaybackCacheConfig.cache_dir` is unset.

These overrides must not be required for normal playback. New cache behavior should add fields to `PlaybackCacheConfig` first, then optionally retain env vars as temporary diagnostics.

## Testing Strategy

### Unit Tests

Add focused tests for:

- cache config defaults and validation
- cache mode resolution for local vs HTTP media
- read-ahead target calculation
- hysteresis pause/resume thresholds
- backbuffer donation calculation
- keyframe-aware range pruning
- cached seek hit within range
- cached seek miss outside range
- HEVC preroll requirements
- BOF/EOF cached flags
- seekable range merge/clamp helpers
- cache buffering percentage calculation
- user pause vs cache pause interaction
- HTTP byte multi-range hit/miss/restart behavior
- raw input rate window calculation

### Integration-Style Backend Tests

Use small local media fixtures if available, or isolated synthetic packet helpers where media fixtures are too heavy:

- seek inside cache does not increment low-level seek count
- seek outside cache increments low-level seek count and creates a new range
- cache pause enters on underrun and exits on recovery
- disk packet cache restores payloads after memory payloads are spilled
- fallback probing reuses the same HTTP cache handle

### Frontend Tests

Add pure tests around rendering helpers:

- timeline range fractions clamp to duration
- overlapping ranges merge
- cached seek target detection
- buffering status mapping from `PlaybackCacheState`

### Manual Verification

Manual checks should cover:

- HTTP Emby video starts and continues prefetching while paused
- progress bar shows cached seekable ranges
- seeking backward inside visible cache is immediate and does not rebuffer
- seeking outside cache shows loading/buffering and then creates a new range
- low bandwidth simulation triggers cache pause and percentage
- user pause during cache pause does not auto-resume after cache recovers
- disk cache file is removed at media close under the default unlink policy
- long playback does not grow memory without respecting configured budgets

## Success Criteria

The implementation is complete when:

- cache configuration is runtime data, not only constants/env vars
- the backend emits a mpv-like unified cache state
- demux packet cache supports multiple seekable ranges and backbuffer limits
- cached seeks avoid low-level FFmpeg seeks when possible
- cache pause is separate from user pause and exposes buffering percent
- HTTP byte cache preserves useful ranges across probes and tail metadata seeks
- the progress UI renders seekable cached ranges
- obsolete HTTP-only frontend state is removed
- tests cover the cache state model, range seeking, pause behavior, and UI mapping

## Open Decisions

- Whether cache settings should be user-configurable through a settings screen or only developer-configurable in the first implementation.
- Whether byte cache ranges should be visible by default or only in the cache popover.
- Whether `cache-secs` should default to mpv's effectively huge value or keep `tiny`'s current practical `120 s` as an app default.
- Whether external subtitle downloads should use the same byte cache machinery in a later pass.
