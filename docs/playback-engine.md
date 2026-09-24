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
`BackendControl::video_output()` 返回不透明的 `VideoOutput`，用于创建
`VideoPresenter`。应用无法访问内部队列、原始解码帧、FFmpeg 指针或 Vulkan 句柄。

所有公开类型从 `tiny_playback` 根部导出；`backend`、`render_host`、
`video_presenter` 等实现模块均为私有模块。公开接口的私有类型泄漏由编译器 lint
检查，外部集成测试和编译失败文档测试验证接口可用性与封装。

引擎的普通、构建和测试依赖均不包含 GPUI，也不直接依赖 `image`。
轨道标签使用 `String`，图像接口只包含像素与尺寸，不依赖应用窗口、主题、
Emby 客户端或用户配置存储。
线程、队列、背压、会话代次与帧生命周期保持原来的实现；应用的
`ShutdownOrder` 仍先释放 presenter，再释放 backend。

最小接入示例见 `crates/tiny-playback/examples/headless.rs`：

```sh
cargo run -p tiny-playback --locked --example headless -- /path/to/video.avi
```

示例仅使用公开接口，在最多 30 秒内消费视频帧，不创建窗口。YUV/HDR 视频仍可能使用
Vulkan/libplacebo；没有 GPU 的环境可使用 BGRA 等走软件转换路径的测试素材。

## 图像与应用适配

`VideoPresenter::render_if_needed` 返回 `Option<BgraImage>`。图像拥有紧密排列、
自上而下的 BGRA8 字节，alpha 为非预乘形式；构造时校验非零尺寸、溢出和字节长度。
像素尺寸可以小于视频源尺寸，布局与宽高比仍使用视频元数据。
HDR/Dolby Vision 映射和 Vulkan 渲染继续在引擎线程内完成，现有 CPU 回读路径不变。

`BgraImage` 不实现 `Clone`，应用通过 `into_bytes()` 接管像素分配。
`src/player/presentation.rs` 将其包装为 GPUI `RenderImage`，视频适配不复制整帧。
GPUI 使用的 `RgbaImage` 容器实际承载 BGRA 字节，适配时不交换颜色通道。

位图字幕通过 `SharedBgraImage` 共享不可变像素，克隆不复制数据，相等性比较对象身份。
引擎负责字幕时间轴、内容与画布坐标；应用负责文字排版、位置、缩放与叠加绘制。
应用仅缓存当前字幕所需的 GPUI 图像：新图像首次显示时复制像素，重复事件和跨 cue
复用同一对象时沿用缓存，绘制时不重建图像。退出、切轨成功、播放失败或页面释放时
回收不再使用的缓存；切轨失败保留原字幕。图集清理保留两次帧回调的延迟。

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

单独测试引擎仍需要 FFmpeg、libplacebo、Vulkan 和音频编译依赖，无需 GPUI。
可使用 `cargo tree -p tiny-playback --locked --edges normal,build,dev`
核对引擎依赖边界。根应用仍使用 GPUI。
Arch 的检查步骤以及 Windows 脚本的 Check、Test、Clippy 模式覆盖整个工作空间。
运行 `cargo run --locked` 仍启动根包应用；发布产物名称和资源目录不受拆分影响。
