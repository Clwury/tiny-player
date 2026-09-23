# 应用与播放引擎

工作空间只有两个 crate，依赖方向为 `tiny-player` → `tiny-playback`。
根目录保留应用包、资源与打包入口，两个包共用根目录的 `Cargo.lock` 和 `target/`。

| 所属包 | 模块与职责 |
| --- | --- |
| `tiny-player` | 窗口、主题、主页、设置、Emby、播放页面、剧集队列、会话上报和偏好持久化 |
| `tiny-playback` | FFmpeg 解码、HTTP 与 demux 缓存、音频、时钟与调度、Vulkan、libplacebo 和视频呈现 |

应用的 `src/player.rs` 保留页面接口，并转导出常用引擎类型。
`src/player/tracks.rs` 通过应用内扩展 trait 将 Emby 媒体流转换为引擎轨道；
语言选择、轨道偏好和 Emby device profile 留在应用包。

## 引擎接口

`BackendLoadRequest` 传递媒体地址、请求头、初始位置、轨道选择及缓存配置。
`BackendControl` / `BackendCommand` 控制播放，`BackendEvent` 返回状态与诊断信息，
`VideoOutputQueue` 连接解码与 `VideoPresenter`。

第一阶段保留 GPUI 的 `RenderImage` 和 `SharedString` 类型。引擎仍依赖 GPUI，
但不依赖应用窗口、主题、Emby 客户端或用户配置存储，也不创建应用窗口。
线程、队列、背压、会话代次与帧生命周期保持原来的实现；应用的
`ShutdownOrder` 仍先释放 presenter，再释放 backend。

## 缓存目录

应用在提交播放请求或更新缓存配置时，通过 `src/player/cache.rs` 注入
`PlaybackCacheConfig.fallback_cache_dir`。该字段仅用于运行时，不写入用户配置。
HTTP 和 demux 两层独立解析目录，优先级均为：

1. 用户显式设置的 `cache_dir`。
2. 对应的 `TINY_HTTP_CACHE_DIR` 或 `TINY_DEMUX_PACKET_CACHE_DIR` 环境变量。
3. 应用提供的默认目录（继续使用 `~/.cache/tiny-player`）。

独立使用引擎时，由调用方提供目录；三者均未提供时不创建磁盘缓存。
内存缓存及已有的磁盘缓存预算、删除策略保持原有规则。

## 构建与验证

`crates/tiny-playback/build.rs` 负责 libplacebo 和 FFmpeg Vulkan 绑定及原生库探测，
绑定从引擎包自己的 `OUT_DIR` 引入。根 `build.rs` 保留 Windows 图标与版本资源。
两个包的版本和许可证保留为各自清单中的具体值；现有打包脚本仍读取根应用包元数据。

```sh
cargo build --locked
cargo test --workspace --locked
cargo test -p tiny-playback --locked
cargo fmt --all -- --check
cargo clippy --workspace --locked --all-targets -- -D warnings
```

单独测试引擎仍需要 FFmpeg、libplacebo、音频和 GPUI 编译依赖。
Arch 的检查步骤以及 Windows 脚本的 Check、Test、Clippy 模式覆盖整个工作空间。
运行 `cargo run --locked` 仍启动根包应用；发布产物名称和资源目录不受拆分影响。
