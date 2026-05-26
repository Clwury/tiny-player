# tiny mpv 风格预读缓存前后端实现 Spec

日期：2026-05-25

范围：`/home/luv/Projects/Rust/tiny/.worktrees/dev` 当前 FFmpeg/GPUI 播放器，以及本地 mpv 源码 `/home/luv/Projects/Application/mpv`。

状态：目标规范。当前工作区已经实现了大部分类型、事件、HTTP byte cache、demux packet cache、缓存暂停和前端展示；本文描述完整实现应达到的语义，并标出仍需媒体级验证或继续收口的差距。

## 1. 背景

`tiny` 现在不是 `libmpv` 壳，而是自有 FFmpeg 后端加 GPUI 前端：

- FFmpeg 后端：`src/player/backend/ffmpeg/`
- HTTP 自定义 AVIO 与 byte cache：`src/player/backend/ffmpeg/avio/`
- demux packet cache：`src/player/backend/ffmpeg/playback_loop/demux_cache.rs`
- 前端播放页、进度条与控制层：`src/player/page/`

mpv 的预读缓存主要是 demux packet cache。它在 demuxer 线程中读包、维护多个可 seek 的 packet range、按前向/后向预算裁剪、允许命中缓存的 seek 不触发底层 seek，并把状态暴露为 `demuxer-cache-state`、`paused-for-cache`、`cache-buffering-state` 等属性。

`tiny` 不能直接照搬 C 数据结构，也不应重新引入 `libmpv`。目标是在当前 FFmpeg 架构上复刻 mpv 的行为语义。

## 2. 参考源码

### tiny

- `src/player/backend.rs`：后端命令、加载请求、`BackendControl` 边界。
- `src/player/backend/events.rs`：缓存配置、缓存状态、后端事件类型。
- `src/player/backend/ffmpeg.rs`：FFmpeg 后端 orchestration、事件合并、兼容状态适配。
- `src/player/backend/ffmpeg/worker.rs`：控制状态、停止、暂停、seek generation。
- `src/player/backend/ffmpeg/format.rs`：FFmpeg format/probe 打开边界。
- `src/player/backend/ffmpeg/avio/cache.rs`：HTTP byte cache。
- `src/player/backend/ffmpeg/avio/callbacks.rs`：AVIO read/seek 回调。
- `src/player/backend/ffmpeg/playback_loop.rs`：主播放循环。
- `src/player/backend/ffmpeg/playback_loop/demux_cache.rs`：demux packet cache。
- `src/player/page/backend_events.rs`：前端应用后端事件。
- `src/player/page/progress.rs`：进度条范围计算、cached seek 判定。
- `src/player/page/controls.rs`：控制层、缓存状态入口与进度条渲染。

### mpv

- `demux/demux.h`：`demux_opts`、`demux_reader_state`、`demux_seek_range`。
- `demux/demux.c`：cache 默认值、读线程、range、prune、cached seek、reader state。
- `demux/cache.c`：append-only packet payload 磁盘缓存。
- `player/playloop.c`：`handle_update_cache`、自动缓存暂停/恢复。
- `player/command.c`：缓存相关属性。
- `DOCS/man/options.rst`：用户选项语义。
- `DOCS/man/input.rst`：属性语义。

## 3. mpv 行为结论

### 3.1 默认值与配置

mpv 的 demux cache 默认值来自 `demux/demux.c::demux_conf`：

- `cache=auto`
- `cache-on-disk=no`
- `demuxer-readahead-secs=1.0`
- `demuxer-hysteresis-secs=0.0`
- `demuxer-max-bytes=150 MiB`
- `demuxer-max-back-bytes=50 MiB`
- `demuxer-donate-buffer=yes`
- `cache-secs=1000h`
- `demuxer-seekable-cache=auto`
- `demuxer-cache-unlink-files=immediate`

`cache=auto` 在 mpv 中基于“是否看起来是网络/慢输入”的启发式启用。启用 cache 后，实际 read-ahead 秒数是 `max(demuxer-readahead-secs, cache-secs)`，但最终仍受 `demuxer-max-bytes` 限制。

### 3.2 状态模型

mpv 的权威状态是 `demux_reader_state`，关键字段：

- `eof`
- `underrun`
- `idle`
- `bof_cached`
- `eof_cached`
- `ts_info.reader/end/duration`
- `ts_per_stream`
- `total_bytes`
- `fw_bytes`
- `file_cache_bytes`
- `seeking`
- `low_level_seeks`
- `byte_level_seeks`
- `ts_last`
- `bytes_per_second`
- `seek_ranges`

`tiny` 的前后端边界必须以一个统一状态表达这些含义，不能继续由 HTTP 专用事件、`BufferedChanged` 和 UI 推导值拼出缓存状态。

### 3.3 Read-Ahead

mpv demux 线程持续读取，直到：

- 选中且 eager 的 stream 有可读 packet；
- lazy/refresh stream 的需求被满足；
- 前向缓存达到时间目标或 byte 目标；
- EOF、停止、seek 中断。

`demuxer-hysteresis-secs` 只在达到限制后生效：读线程进入 idle 后，直到剩余前向缓存降到 hysteresis 阈值以下才恢复预读。

### 3.4 Seekable Cache

mpv 维护多个 `demux_cached_range`。每个 range 有每条 stream 的 packet queue、全局 packet 顺序、`seek_start`、`seek_end`、`is_bof`、`is_eof`。

cached seek 条件：

- 目标落入 `seek_start..seek_end`；或
- 目标早于 `seek_start` 且 range 标记 `is_bof`；或
- 目标晚于 `seek_end` 且 range 标记 `is_eof`。

命中后 mpv 不执行底层 demuxer seek，而是把各 stream 的 reader head 移到安全 packet。非 HR seek 会向前调整到视频 keyframe，避免视频欠读而音频先跑。

如果命中的不是当前 append range，mpv 会切换 current range，并排一个低层 resume seek 到 range 末尾附近，让未来 append 回到正确位置。这是 `tiny` 完整实现中必须具备的“read range 与 append range 分离”语义。

### 3.5 Backbuffer 与 Prune

mpv 把缓存 byte 分成：

- `fw_bytes`：当前 reader 后面的前向数据；
- backbuffer：当前 reader 之前、可供回退 seek 的数据；
- `total_bytes`：所有 packet queue 的估算 byte。

预算规则：

- 前向数据受 `demuxer-max-bytes` 约束。
- 后向数据受 `demuxer-max-back-bytes` 约束。
- `demuxer-donate-buffer=yes` 时，未用完的前向预算可以借给 backbuffer，但保留 1 byte guard，避免读线程卡死。
- prune 从 least-recently-used 旧 range 开始。
- seekable cache 开启时，prune 应尽量在 keyframe/recovery-point 边界裁剪，更新 `seek_start/seek_end`。
- 不能裁掉当前 reader 之后仍需解码的 packet。

### 3.6 Disk Packet Cache

mpv 的 `demux/cache.c` 是 packet payload append-only 临时文件：

- packet metadata 仍在内存；
- 文件空间不复用；
- 关闭媒体时删除文件，默认 `immediate` 是创建后立刻 unlink，只保留 fd；
- `file_cache_bytes` 报告文件 append 到过的大小，包括已经不可达的旧 payload。

### 3.7 Cache Pause

mpv 的 `player/playloop.c::handle_update_cache` 语义：

- 用户暂停和缓存暂停分离，有效暂停是二者 OR。
- `cache-pause` 只对正向播放生效。
- 初始缓存暂停由 `cache-pause-initial` 控制，可以在播放 restart 前进入。
- restart 后进入 cache pause 需要真实低缓存，并且发生 demux underrun；mpv 还要求 audio/video output underrun，或者没有对应检测器。
- 恢复条件是缓存时长达到 `cache-pause-wait`，或 demux idle/EOF 表示无法继续预读。
- buffering 百分比是 `cache_duration / cache_pause_wait`，暂停中 capped below 100。

## 4. tiny 当前状态快照

已基本具备：

- `PlaybackCacheConfig`、`PlaybackCacheState`、`DemuxCacheState`、`ByteCacheState`。
- `BackendEventKind::CacheStateChanged`、`PausedForCacheChanged`、`CacheBufferingChanged`。
- `BackendLoadRequest.cache_config` 和 `BackendCommand::SetCacheConfig`。
- HTTP per-load cache 在 probe fallback 间复用。
- HTTP byte cache 有 retained ranges、raw input rate、byte-level seek count、LRU memory/disk prune，并且 AVIO 直接构造统一 `ByteCacheState` 上报 byte-cache 增量。
- Demux cache 的 `read_range`、`append_range` 和 archived ranges 已统一存入 `RangeId` map，具备 cached seek hit/miss、BOF/EOF flags、stream kind、forward/back byte budget、hysteresis、按 per-stream seekable start 选择 archived queue head 的 keyframe/timestamp-aware prune、packet disk cache。
- demux seekable ranges 和 cached seek 命中现在会把 video recovery/keyframe timeline 与已选 eager audio 覆盖区间求交；即使 video packet 存在，只要 audio 缺包也会拒绝 cached seek。
- demux packet 被 decoder 读取后也会按节流规则重新发送 cache state，因此 `reader_pts`、`cache_duration`、`forward_bytes` 不再只依赖未来 append 事件更新。
- demux aggregate 与 per-stream 的 `cache_duration` 在 cache end 早于 reader position 时保持 `None`，不再把无效时间顺序伪装成 0。
- demux EOF 状态现在遵循 mpv 的 EOF 即 idle 语义，包括 archived cached seek 后由 detached append range 触达的 effective EOF。
- seek 和 track selection restart 会清理旧的 output-underrun 标记，因此 post-restart cache pause 必须观察到本次 restart 后的新 output underrun 才能进入 mpv 风格 cache-pause gate。
- seek/restart 路径现在保留 `user_paused`，只清除 cache pause；前端播放/暂停按钮也改为由用户暂停状态驱动，而不是由 effective pause 驱动，因此单纯 cache pause 不会被显示成用户暂停。
- `DemuxCacheState` 已携带 mpv 风格 `seeking` 状态，用于 queued/in-progress low-level demux seek，包括 archived cached-range 命中后的 continuation seek。
- `FfmpegBackend` 不再用旧 merged state 保留 `cached_seeks`；cached-seek 计数以最新 demux cache state 为准。
- `FfmpegControl` 已拆分 user pause 与 cache pause。
- cache pause 已有 audio output underrun gate，以及视频队列为空且等待 demux 数据时的 video output starvation marker。
- 前端存储统一 cache state，进度条可画 demux seekable ranges 和 byte underlay，控制层有 cache status affordance。

仍需验证或继续收口的 mpv 语义：

- demux range 模型已有显式 `read_range`、`append_range`、range id 和 continuation seek；仍需长播放和高频 seek 的手工验证。
- HTTP byte cache 现在是一个 active 下载 range 加 retained ranges，并额外有 queued multi-worker side range 下载通道；cached/probe read 与 foreground playback miss 的未缓存 offset，包括已裁掉 active range 之前的 offset，都可在不立即重启 active playback range 的情况下排独立 side range。side range 创建会立即上报 byte-cache active 状态；side worker 只下载自己的 range-request budget，不会一路续读到媒体 EOF。playback seek 离开 active byte range 时，旧 active range 会降级为 retained cache，继续服务 cache hit；foreground side range 完成后，active worker 会从该 range 末尾继续预读；EOF 和最后一个 side range 完成时会通过统一 `CacheStateChanged` 立即报告 byte-cache idle。
- `PlaybackCacheState` 大部分来自真实 cache internals，但仍保留少量 `FfmpegBackend` 合并 byte/demux 增量的兼容适配。
- demux prune 已能在 LRU archived range 内按每个 stream 自己的 seekable start 选择最早 queue head，并按 video recovery/keyframe 或非 video timestamp 边界裁剪一个安全 packet run；subtitle 等稀疏流被裁后会用 pruned-watermark 抬高 cached seekable range 起点；仍需真实媒体级验证。
- cache pause 已有 audio underrun gate 和 video starvation marker；仍需低带宽、seek restart 等媒体级手工验证。
- `BufferedChanged`、`Buffering` 仍作为兼容/粗粒度事件存在，应只由统一 cache state 派生或逐步移除。

## 5. 目标与非目标

### 目标

- 在当前 FFmpeg 后端上实现 mpv 风格预读缓存。
- demux packet cache 成为时间 seek 的权威依据。
- HTTP byte cache 仅作为 AVIO 输入优化和诊断 underlay。
- 后端向前端暴露单一、mpv-like 的 cache state。
- cached seek 命中时不触发 FFmpeg low-level seek。
- backbuffer、range prune、disk packet cache、cache pause 都可配置并可测试。
- 前端进度条和 seek UX 能反映缓存命中/未命中。

### 非目标

- 不引入 `libmpv`。
- 不实现 mpv backward playback。
- 不实现 playlist prefetch。
- 不要求第一版提供设置页面或持久化用户配置。
- 不让 cache 文件跨媒体复用。
- 不用 HTTP byte range 证明“时间 seek 一定命中”，时间 seek 只看 demux packet range。

## 6. 公共后端 API

### 6.1 配置类型

`src/player/backend/events.rs` 中的 `PlaybackCacheConfig` 是权威配置。

```rust
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

实现要求：

- `Default` 对齐 mpv 默认值，除非产品层显式覆写。
- `normalized()` 只做合法化，不做隐藏策略。
- cache 是否启用必须通过 resolved config 表达，不依赖散落常量。
- env vars 只允许作为开发调试 override，并需要文档列出。

`mode` 解析：

- `Disabled`：cache pause disabled；seekable cache disabled，除非 `seekable_cache=Enabled` 明确打开；backbuffer 预算为 0。
- `Enabled`：使用 `max(demuxer_readahead_secs, cache_secs)`。
- `Auto`：网络/慢输入启用 cache；本地文件仅保留 demux thread 的短 read-ahead。

`seekable_cache` 解析：

- `Disabled`：不保留 seekable backbuffer。
- `Enabled`：尽量保留 seekable ranges。
- `Auto`：跟随 cache active，与 mpv 一致。

### 6.2 状态类型

`BackendEventKind::CacheStateChanged(PlaybackCacheState)` 是唯一权威 cache state 事件。

```rust
pub struct PlaybackCacheState {
    pub demux: DemuxCacheState,
    pub byte: Option<ByteCacheState>,
    pub paused_for_cache: bool,
    pub buffering_percent: Option<u8>,
}
```

`DemuxCacheState` 必须满足：

- `seekable_ranges` 来自 demux packet ranges，不来自 HTTP byte ranges。
- `cache_end/cache_duration/reader_pts` 来自选中 eager streams 的最短 forward window，subtitle 空窗口不能压低 video/audio 的有效缓存。
- `bof_cached/eof_cached` 来自 range flags，不从 `start <= 0` 或 `end >= duration` 粗略推导。
- `total_bytes` 是 packet metadata/payload 估算总量；`forward_bytes` 是 reader 后方前向 byte。
- `file_cache_bytes` 是 append-only disk packet cache 文件大小，不是当前可达 payload 大小。
- `cached_seeks` 只在无 low-level FFmpeg seek 的 user seek 命中时增加。
- `low_level_seeks` 只在实际排入或执行 `FormatContext::seek_stream` 等 demuxer seek 时增加。
- `byte_level_seeks` 从 AVIO/HTTP byte cache 汇总。
- `idle` 表示 demux 线程当前不读或 EOF。
- `underrun` 表示选中 eager stream 无法提供 reader 需要的 packet，且 demux 线程不是 idle。

`ByteCacheState` 必须满足：

- `ranges` 是 byte fraction ranges，只在 `content_length` 已知时供 UI underlay 使用。
- `cached_bytes` 包含 memory retained byte ranges 与 disk byte ranges 的可达总量。
- `raw_input_rate` 是最近窗口内网络原始输入速率。
- `byte_level_seeks` 是 AVIO 跨 range seek 计数。

### 6.3 事件

必需事件：

- `CacheStateChanged(PlaybackCacheState)`
- `PausedForCacheChanged(bool)`
- `CacheBufferingChanged(Option<u8>)`

兼容事件：

- `BufferedChanged(Option<f64>)` 可以保留，但只能从 `PlaybackCacheState.demux.cache_end` 派生。
- `Buffering(bool)` 只能表示粗粒度加载/seek，不应携带 cache 细节。

事件节流：

- 连续预读期间最多按固定间隔发送状态。
- seek、range create/prune、EOF、underrun、cache pause enter/exit、disk cache failure 必须立即发送状态。

### 6.4 命令

- `BackendCommand::SetCacheConfig(PlaybackCacheConfig)`
- `BackendControl::cache_state() -> Option<PlaybackCacheState>`

live update 要求：

- 可即时生效：read-ahead 秒数、hysteresis、byte budgets、cache pause 选项。
- 可延迟到未来 packet：disk cache enable。
- disk cache disable：停止写新 packet，但旧文件保留到媒体关闭。
- `cache_dir` 与 unlink policy：下一次创建 cache 文件时生效。

## 7. 后端架构 Spec

### 7.1 控制状态

`FfmpegControl` 必须包含：

- `user_paused`
- `cache_paused`
- `effective_paused()`
- sticky demux/output underrun markers
- seek generation

规则：

- 用户 pause/resume 只改变 `user_paused`。
- cache pause 只改变 `cache_paused`。
- 清除 cache pause 不得清除 user pause。
- 输出 underrun marker 需要 sticky 到 cache 恢复或 seek/restart 清理。

### 7.2 加载与配置流

加载流程：

1. 前端创建 `BackendLoadRequest`，携带 resolved `PlaybackCacheConfig`。
2. `FfmpegBackend` 保存 config，创建 worker。
3. HTTP URL 创建 per-load `CachedInputSource`，probe fallback 复用同一 byte cache。
4. format open 完成后，把 duration/content length 反馈给 byte cache。
5. demux packet cache 以 start position 创建初始 append range。
6. 后端发送初始 `CacheStateChanged`。
7. 如果 `demuxer_cache_wait` 或 `cache_pause_initial` 需要等待，则在播放 restart 前预读。

### 7.3 HTTP Byte Cache

HTTP byte cache 是 AVIO 输入层，不是时间 seek 权威。

目标模型：

```rust
struct ByteCacheRange {
    start: u64,
    end: u64,
    storage: Memory | Disk,
    kind: Playback | TailMetadataProbe | Probe,
    last_used_generation: u64,
}

struct HttpByteCache {
    ranges: RangeSet<ByteCacheRange>,
    active_downloads: Vec<DownloadRange>,
    reader_offset: u64,
    content_length: Option<u64>,
    byte_level_seeks: u64,
    raw_input_rate_window: InputRateWindow,
}
```

必需行为：

- 读请求命中任意 memory/disk range 时立即返回。
- 读请求 miss 时，在请求 offset 创建或重启下载 range。
- probe fallback 和 tail metadata seek 产生的 byte ranges 进入普通 range set。
- LRU prune 覆盖所有 retained memory ranges。
- disk byte cache 写失败只关闭本 session 的 disk 写入，不影响播放。
- disk byte ranges 参与 `ByteCacheState.ranges` 和 `cached_bytes`。
- `byte_level_seeks` 在 AVIO seek 移出当前 contiguous readable range 时增加。
- `raw_input_rate` 用最近约 1 秒网络读入 byte 计算。

完成标准：

- 同一媒体多次 probe 不重复下载初始段。
- backward byte seek 命中 retained/disk range 不重启网络下载。
- tail metadata probe 可被后续读取复用。
- 预算压力下被淘汰的是 LRU range，而不是仅按 offset 最旧裁剪。

如果第一阶段仍保留单 active downloader，必须在状态和代码注释中明确这是中间实现；完整目标是 range set 与下载调度解耦。

### 7.4 Demux Packet Cache Range 模型

最终不要用 “active + archived” 作为核心抽象。应改为显式 range model：

```rust
type RangeId = u64;
type PacketId = u64;

struct DemuxPacketCacheState {
    packets: HashMap<PacketId, CachedDemuxPacket>,
    ranges: BTreeMap<RangeId, DemuxCachedRange>,
    read_range: RangeId,
    append_range: RangeId,
    next_range_id: RangeId,
    next_packet_id: PacketId,
    reader_nsecs: u64,
    demuxer_nsecs: Option<u64>,
    config: ResolvedCacheConfig,
    counters: CacheCounters,
}

struct DemuxCachedRange {
    id: RangeId,
    global_order: VecDeque<PacketId>,
    stream_queues: BTreeMap<c_int, VecDeque<PacketId>>,
    seek_start_nsecs: Option<u64>,
    seek_end_nsecs: Option<u64>,
    is_bof: bool,
    is_eof: bool,
    last_used_generation: u64,
}
```

核心要求：

- `read_range` 是 decoder 当前读 packet 的 range。
- `append_range` 是 demuxer 当前追加 packet 的 range。
- 两者可以不同。用户从旧 range 命中 cached seek 时，decoder 读旧 range；demuxer 需要排 continuation low-level seek 到旧 range end 附近，以便未来 append 与 read range 对齐。
- range metadata 必须能表达 BOF/EOF，不依赖 duration 猜测。
- per-stream queue 是 seek/prune 的基础，不能只靠全局 video timeline。

### 7.5 Demux Read-Ahead 算法

demux 线程循环：

1. 处理 pending seek 或 continuation seek。
2. 判断是否应继续读。
3. 调用 `av_read_frame`。
4. 把 packet 转换为 `CachedDemuxPacket`，追加到 `append_range`。
5. 更新 range seek bounds、stream windows、byte counters。
6. prune。
7. emit throttled cache state。

继续读条件：

- 任一选中 eager video/audio stream 没有 forward packet；
- selected lazy stream 被强制读到目标；
- stream 正处于 refreshing；
- forward duration 小于目标；
- hysteresis 当前允许 resume。

暂停读条件：

- `forward_bytes >= demuxer_max_bytes`；
- forward duration 达到 read-ahead target；
- hysteresis active 且 forward duration 大于 resume threshold；
- EOF；
- shutdown；
- pending seek。

`cache_duration` 计算：

- 对选中 video/audio/subtitle 分别计算 reader 到 stream queue end 的 forward window。
- 有 video/audio 窗口时，空 subtitle window 不得使总 cache duration 变成 0。
- 总 duration 使用最短选中 eager stream window。
- timestamp 未知时用 `None`，不要用 0 伪装未知。

### 7.6 Cached Seek

seek 流程：

1. target seconds 转 timeline nsecs。
2. 如果 seekable cache disabled，直接 miss。
3. 遍历 ranges，找满足 target 与 BOF/EOF 语义的 range。
4. 在命中 range 中找安全 video anchor：
   - keyframe 或 recovery point；
   - target 前方最近；
   - HEVC 保留现有 preroll 要求；
   - 找不到则 miss。
5. 为每个 selected stream 移动 reader cursor 到合适 packet。
6. 更新 `read_range`、`reader_nsecs`、generation。
7. 如果 `read_range != append_range`，排 continuation seek 到 range end 附近。
8. 增加 `cached_seeks`。
9. flush decoder 并从 cached packet 重启。
10. 发送 `CacheStateChanged`。

miss 流程：

1. 保留当前 range，如果 seekable cache 关闭则丢弃。
2. 创建 fresh append/read range。
3. 排 low-level seek。
4. 增加 `low_level_seeks`。
5. 清理 EOF、hysteresis、cache buffering 中间态。
6. 发送状态。

### 7.7 Prune 与预算

预算计算：

```text
forward_bytes = bytes(read cursor..append end in append/read-active path)
backward_bytes = total_bytes - forward_bytes
back_limit = demuxer_max_back_bytes
if donate_buffer && back_limit > 0:
    back_limit += max(0, demuxer_max_bytes - (forward_bytes + 1))
```

prune 策略：

- 当前 reader 后面的 packet 不可裁。
- 优先裁 LRU old range。
- seekable cache enabled 时，一次裁到下一个 keyframe/recovery boundary，并更新 range seek start。
- 非 seekable 或没有 keyframe boundary 时，可以更激进裁掉旧 packet/run。
- range 变空或 `seek_start` 失效时移除。
- 每次 prune 后重算 `seekable_ranges`、BOF/EOF flags、byte counters。

### 7.8 Disk Packet Cache

实现要求：

- `PlaybackCacheConfig.disk_cache` 控制启用。
- `cache_dir` 与 `unlink_files` 来自 config。
- 默认 `Immediate` 创建后 unlink。
- payload append-only 写入。
- metadata 保留在内存，并参与内存预算。
- 写失败停止未来 disk 写入，已有 disk payload 仍可读；如果不可读，返回明确错误并停止播放。
- `file_cache_bytes` 报告 append 文件大小。

### 7.9 Cache Pause

进入条件：

- `cache_pause=true`
- 正向播放
- demux 不 idle 且非 EOF
- cache duration < `cache_pause_wait`
- post-start：发生过 demux underrun
- post-start：发生过 audio 或 video output underrun；如果某输出链没有检测器，则可保守把 demux wait 视为 output starvation
- initial/restart 前：`cache_pause_initial=true` 可跳过 output underrun 要求

保持状态：

- `cache_paused=true`
- `PausedForCacheChanged(true)`
- `CacheBufferingChanged(Some(percent))`
- percent capped at 99 while still paused
- demux thread 继续预读

退出条件：

- cache duration >= `cache_pause_wait`
- 或 demux idle/EOF/byte limit 表明不能继续预读
- 或 stop/unload/fatal

退出动作：

- `cache_paused=false`
- `CacheBufferingChanged(None)`
- `PausedForCacheChanged(false)`
- 如果 `user_paused=true`，有效播放仍暂停。

### 7.10 后端事件合并

`FfmpegBackend` 目前会合并 partial byte/demux cache events，并对 `BufferedChanged` 做 fallback 推导。完整实现应收敛为：

- demux cache event：携带完整 `PlaybackCacheState.demux`。
- HTTP byte cache event：携带 `PlaybackCacheState.byte` 增量。
- `FfmpegBackend` 只做结构合并，不合成 BOF/EOF、seekable ranges、cache duration 等权威字段。
- compatibility `BufferedChanged` 只从最新 demux `cache_end` 发送。

## 8. 前端 Spec

### 8.1 状态

`PlaybackTimelineState` 应保留：

- `cache_state: Option<PlaybackCacheState>`
- `paused_for_cache: bool`
- `cache_buffering_percent: Option<u8>`
- `buffered_until: Option<f64>` 仅作为兼容 fallback
- `pending_seek_position`
- `pending_seek_keeps_frame`

不得恢复旧的 HTTP-only UI 状态字段。

### 8.2 事件处理

`PlaybackPage::apply_backend_event`：

- `CacheStateChanged`：保存完整 state，并从 `state.demux.cache_end` 更新 `buffered_until` fallback。
- `PausedForCacheChanged`：只更新 cache pause 原因。
- `CacheBufferingChanged`：更新百分比。
- `BufferedChanged`：只作为旧 fallback。
- `Buffering(bool)`：只控制粗粒度 loading/seeking，不覆盖 cache pause。

暂停时如果 cache state 显示 demux 或 byte cache 仍在工作，继续安排 backend poll，直到 idle/EOF。

### 8.3 进度条

渲染层：

1. track background；
2. byte cache underlay，可选，薄且低透明度；
3. demux seekable ranges，权威可 seek 缓存；
4. played progress；
5. drag target hint。

规则：

- demux ranges clamp 到 `[0, duration]`。
- overlapping/相邻 ranges 合并。
- 没有 demux ranges 时，才使用 `buffered_until` 画单段 fallback。
- byte ranges 只有 `content_length` 与 duration 已知时才显示。
- byte underlay 不参与 cached seek 判定。

### 8.4 Seeking UX

拖动时：

- target 命中 `cache_state.demux.seekable_ranges`：用 cached-hit 样式。
- 未命中：用 miss/warning 样式。

提交 seek 时：

- 命中预期 cached range：保留当前 frame，不立即显示 loading overlay。
- 未命中：显示正常 seeking/loading，等待后端确认。
- 最终以后端 `CacheStateChanged`、`Buffering`、frame arrival 为准。

### 8.5 Buffering Overlay

当 `paused_for_cache=true`：

- 显示缓存中状态。
- 如果 `cache_buffering_percent` 存在，显示百分比。
- play/pause 按钮仍表示用户暂停状态；用户 resume 后，如果 cache pause 未恢复，播放仍被缓存暂停 hold。

### 8.6 Cache Status Affordance

控制层保留小型图标入口，不喧宾夺主。popover/tooltip 可展示：

- input speed；
- demux cache duration；
- byte cached bytes；
- disk cache bytes；
- idle/running；
- cached/low-level/byte-level seek counters；
- paused-for-cache percent。

## 9. 实现阶段

### Phase 1：类型与事件边界

状态：基本完成。

- 配置、状态和事件类型。
- load request 携带 cache config。
- frontend 消费 `CacheStateChanged`。

完成标准：

- 所有 cache UI 都能从统一状态读取。
- HTTP-only 事件不再出现在 frontend。

### Phase 2：Runtime Config

状态：部分完成。

- 把 demux/HTTP 常量迁移到 `PlaybackCacheConfig`。
- 保留 env vars 为 debug override。
- 支持 live `SetCacheConfig`。
- HTTP byte cache 的 memory/chunk budget 已来自 `PlaybackCacheConfig`，`TINY_HTTP_CACHE_*` 只作为调试覆盖。
- HTTP Range request size 与 HTTP/demux disk-cache byte budget 已来自 `PlaybackCacheConfig`，env vars 只保留为调试覆盖。
- `cache=auto` 会在配置 demux packet cache 前按输入是否可缓存解析：HTTP/HTTPS 输入启用 cache-active 行为，本地输入保持普通 `demuxer_readahead_secs`，除非显式强制 cache 或 seekable cache。
- `cache_secs=0` 按 mpv 语义保留为合法值，cache-active 时会回落使用 `demuxer_readahead_secs`，不会静默恢复到巨大默认值。
- `demuxer_max_bytes=0` 按 mpv 语义保留为合法值；如果选中的 eager stream 没有 forward packet，demux 线程仍可继续读包，不会把 byte limit 当成绝对停读条件。

剩余：

- 明确 app 默认与 mpv 默认的差异。

### Phase 3：HTTP Multi-Range Byte Cache

状态：部分完成。

- 已有 retained ranges、LRU、disk byte ranges、raw rate、byte seek count，以及不打断 active playback range 的 queued side range downloader，cached/probe/foreground read miss 可在 active range 前后排独立 side range。
- side range 创建会及时上报 byte-cache active，下载已被限制在自己的 range-request budget 内；foreground side range 完成后会调度 active worker 从该 range 末尾继续预读；EOF/最后一个 side range 完成会立即上报 byte-cache idle；AVIO 已直接上报 `ByteCacheState`，不再保留旧 HTTP DTO；`cache=Disabled` 会绕过 HTTP byte cache，交回 FFmpeg 原生 HTTP；剩余更大范围的媒体级验证和用户可见 byte-cache option flow。

完成标准：

- 任意 retained/disk range hit 不重启下载。
- 多个离散 byte ranges 可同时存在、报告、LRU prune。
- tail metadata probe 与 probe fallback 复用明确可测。

### Phase 4：Demux Range Model

状态：大部分完成。

- 已从 active + archived 迁移到统一 `RangeId` map 下的 `read_range`/`append_range`/archived ranges。
- cached seek 命中旧 range 时，decoder 读旧 range，demuxer continuation seek 到 range end。
- 已新增 archived range 的 per-stream queue-head prune：在 LRU old range 内选择 seekable start 最早的 stream，再按 video recovery/keyframe 或非 video timestamp 边界裁一个安全 packet run，减少只为回收旧 audio/subtitle packet 而缩短 video seekable range 的情况；subtitle 等稀疏流被裁后会用 pruned-watermark 抬高 cached seekable range 起点。剩余长播放/高频 seek 手工验证。
- demux cache 现在基于实际读到的 packet payload 统计 raw input rate；HTTP byte cache 有网络速率时，后端合并仍优先使用 byte cache 的真实输入速率。
- `ts_last` 现在来自最近读到的 demux packet 时间戳，并在低层 seek 请求时清空，对齐 mpv 的 `debug-ts-last` reset 语义。
- packet read 后会发送节流 cache state，reader 位置和前向缓存 byte/duration 会随消费推进。

完成标准：

- current range 与 append range 可分离。
- in-cache seek 不增加 low-level seek。
- continuation seek 只为后续 append 对齐，不破坏本次 cached seek 的低延迟语义。

### Phase 5：Cache Pause

状态：部分完成。

- user pause/cache pause 已拆分。
- audio output underrun gate 已有。
- video starvation marker 已有。
- cache-pause wait target、live wait target/disable 变更、EOF/idle 提前恢复已有自动测试覆盖。
- 剩余手工低带宽、seek restart 和真实媒体 underrun 路径验证。

完成标准：

- initial cache pause、post-start cache pause、seek restart cache pause 均符合 mpv。
- 用户暂停叠加 cache pause 不会自动误恢复。

### Phase 6：Frontend Polish

状态：大部分完成，已覆盖 timeline ranges、byte-cache underlay、compact cache status、cache active 时 paused polling、旧 HTTP-only UI/event path 移除、提交 cached seek 时保留当前帧，以及拖动时 cached-seek 高亮。

- demux ranges、byte underlay、cache status、drag-time hit/miss 已有。
- 剩余：对 cache pause 文案/图标、idle/running 状态、异常状态做最终 polish。

### Phase 7：清理

状态：进行中。

- 降低 `BufferedChanged`/`Buffering` 的 cache 语义权重。
- 旧 HTTP DTO 已从事件层和 AVIO 状态上报路径移除；AVIO 直接构造 `ByteCacheState`。
- 增加开发文档与手工验证清单。

## 10. 开发调试覆盖项

`PlaybackCacheConfig` 是运行时行为的权威来源。以下环境变量只作为本地调试覆盖项，在提升为 typed config 前不应暴露为用户设置：

- `TINY_HTTP_CACHE_MEMORY_BYTES`：覆盖 normalized 之后的 HTTP byte cache 内存预算。
- `TINY_HTTP_CACHE_CHUNK_BYTES`：覆盖 HTTP worker read chunk size，并限制在 `64 KiB..16 MiB`。
- `TINY_HTTP_CACHE_RANGE_REQUEST_BYTES`：覆盖 HTTP Range 请求大小，并限制在 `64 KiB..128 MiB`，且不小于配置的 chunk size。
- `TINY_HTTP_CACHE_READAHEAD_SECS`：覆盖 HTTP byte cache 预读目标时长。
- `TINY_HTTP_CACHE_HYSTERESIS_SECS`：覆盖 HTTP byte cache resume hysteresis。
- `TINY_HTTP_CACHE_MAX_BYTES`：覆盖 HTTP 最大预读字节数。
- `TINY_HTTP_CACHE_DISK_BYTES`：在 `PlaybackCacheConfig.disk_cache` 启用时，覆盖 HTTP disk byte cache 的 `PlaybackCacheConfig.disk_cache_max_bytes`。
- `TINY_HTTP_CACHE_DIR`：在 `PlaybackCacheConfig.cache_dir` 未设置时覆盖 HTTP disk byte cache 目录。
- `TINY_DEMUX_PACKET_CACHE_ON_DISK`：即使 runtime config 未启用 disk cache，也强制启用 demux packet payload disk cache。
- `TINY_DEMUX_PACKET_CACHE_BYTES`：覆盖 demux packet payload disk cache 的 `PlaybackCacheConfig.disk_cache_max_bytes`。
- `TINY_DEMUX_PACKET_CACHE_DIR`：在 `PlaybackCacheConfig.cache_dir` 未设置时覆盖 demux packet payload disk cache 目录。

正常播放不能依赖这些变量。新增 cache 行为应优先进入 `PlaybackCacheConfig`，env var 只保留为临时诊断入口。

## 11. 测试计划

### Backend Unit Tests

必须覆盖：

- config defaults 与 normalization；
- cache mode/seekable mode resolve；
- read-ahead target 与 hysteresis resume；
- forward/back bytes 与 donate buffer；
- BOF/EOF flags；
- cached seek 当前 range 命中；
- cached seek retained/old range 命中；
- cached seek miss；
- HEVC preroll；
- keyframe-aware prune；
- disk packet cache write/read/unlink policy；
- cache pause initial；
- cache pause post-start demux + output underrun gate；
- user pause 与 cache pause 叠加；
- cache buffering percent capped below 100；
- EOF/idle 提前恢复 cache pause。

### HTTP Tests

必须覆盖：

- probe fallback reuse；
- retained range hit；
- disk range hit；
- LRU prune；
- tail metadata probe reuse；
- byte-level seek count；
- raw input rate；
- disk write failure disables disk cache only。

### Frontend Tests

必须覆盖：

- demux range clamp/merge；
- byte range clamp/merge；
- cached seek target detection；
- miss target warning；
- cache status segment formatting；
- paused-for-cache overlay mapping；
- `buffered_until` fallback 只在没有 range 时使用。

### Integration/Manual

手工验证：

- Emby HTTP 播放启动和持续预读；
- 暂停时仍可预读；
- 低带宽触发 cache pause 与百分比；
- 用户暂停期间 cache pause 恢复不自动播放；
- backward seek 命中缓存立即恢复；
- outside-cache seek 触发 low-level seek 并创建新 range；
- 长时间播放内存稳定；
- disk cache `Immediate/WhenDone/Never` 行为；
- seek 后 restart 的 cache pause 行为；
- 音频与视频输出 underrun 触发路径。

## 12. 成功标准

实现完成必须同时满足：

- 配置是 runtime data，不是隐藏常量/env-only。
- 后端统一发送 mpv-like `PlaybackCacheState`。
- demux packet cache 支持多个 seekable ranges。
- `read_range` 与 `append_range` 分离，支持旧 range cached seek 与 continuation append。
- cached seek hit 不调用 low-level FFmpeg seek。
- backbuffer 和 prune 遵守前向/后向预算、donate buffer、keyframe/recovery boundary。
- disk packet cache 是 config-driven append-only temp cache。
- cache pause 与 user pause 完全分离，并符合 mpv initial/post-start 规则。
- HTTP byte cache 复用 probe/tail ranges，并报告 byte diagnostics。
- 前端时间 seek 判定只依赖 demux seekable ranges。
- 旧 HTTP-only UI 状态被移除或私有化。
- 自动测试和手工验证覆盖上述关键行为。

## 13. 开放决策

- app 默认是否完全采用 mpv 的 1000h `cache_secs`，还是保留较小产品默认但显式写入 config。
- byte underlay 默认显示还是仅在 cache popover/调试模式显示。
- 是否在第一版提供用户设置入口，还是仅用开发配置。
- 外挂字幕/外部音轨是否后续接入同一 byte cache。
- full independent multi-download byte cache 是否一次完成，还是先把现有单 active downloader 抽象成 range scheduler 后再扩展。
