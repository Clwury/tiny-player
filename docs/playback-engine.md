# 应用与播放引擎

工作空间只有两个 crate，依赖方向为 `tiny-player` → `tiny-playback`。
根目录保留应用包、资源与打包入口，两个包共用根目录的 `Cargo.lock` 和 `target/`。

| 所属包 | 模块与职责 |
| --- | --- |
| `tiny-player` | 窗口、主题、主页、设置、Emby、播放页面、剧集队列、会话上报和偏好持久化 |
| `tiny-playback` | FFmpeg 解码、HTTP 与 demux 缓存、音频、时钟与调度、Vulkan、libplacebo 和视频呈现 |

应用的 `src/player.rs` 保留页面接口，并转导出常用引擎类型。
`src/media/tracks.rs` 通过应用内扩展 trait 将 Emby 媒体流转换为引擎轨道；
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

## Demux 慢读日志

`av_read_frame` 单次耗时达到 200 ms 时记录慢读事件。`read_phase` 区分 `startup`、
`seek_preroll` 与 `steady`；启动或低层 seek 后，只有首个有效数据包被当前输入代次的缓存
接纳，才结束首包等待。重试、被取消的流和重复包不会重置等待时间，也不会提前结束该阶段。

首包等待累计不足 2 秒且读取成功时，以 DEBUG 记录 `startup_read` 或 `seek_preroll_read`。
累计等待达到 2 秒后，若未处于缓存暂停且前向缓存不超过 250 ms，仍以 WARN 记录
`first_packet_delayed`；正常读取阶段同样保留 `low_water_stall` 告警。
已有超过 250 ms 的前向缓存或处于缓存暂停时，沿用原有 DEBUG 分类。读取错误不享有
成功首包的宽限，实际错误继续由现有重试及错误处理流程上报。

日志同时包含 `first_packet_wait_ms`、本次读取的 `demux_input_generation` 和当前缓存的
`current_demux_input_generation`。
跨 seek 后已失效的旧读取归为 `superseded_input`，以 DEBUG 记录，避免将新位置的空缓存
归因于旧读取。慢读事件在本次包入队之前记录；缓存为空或读取成功均不能单独证明画面卡顿
或渲染成功，仍需结合解码、输出缓冲和渲染日志判断。

## 视频解码策略

默认使用 `Auto` 模式，按当前选择的 GPU 和平台依次尝试：

| 平台 / GPU | 候选顺序 |
| --- | --- |
| Linux NVIDIA | Vulkan → VA-API 映射 Vulkan → 软件解码 |
| Linux 非 NVIDIA | VA-API 映射 Vulkan → Vulkan → 软件解码 |
| Windows | Vulkan → 软件解码 |

开发设置的“自动”使用 `AutoWithCopyback`（配置值 `auto_with_copyback`），在 Linux
软件解码候选之前保留 VA-API 回传内存。普通 Auto 不尝试回传内存。两种策略均随偏好保存，
打开另一种设置窗口不会切换策略，恢复和 seek 保持原候选列表与策略。

通过选中 Vulkan physical device 的 vendor ID 判断 NVIDIA，不扫描整机显卡列表。
VA-API 匹配同一 GPU 的 DRM render node；指定 `TINY_VAAPI_DEVICE` 时 Auto 的 Vulkan
设备也从该 DRM 节点派生。设备选择未知时使用非 NVIDIA 顺序；无法匹配节点则跳过
对应 VA-API 候选，Vulkan 不可用时允许由 FFmpeg 选择默认 VA-API 设备。
每个硬解候选严格协商硬件帧，不能在候选内部静默软解。打开失败、格式协商失败、
VA-API 映射及硬件渲染失败会继续消耗当前播放的候选列表，重新定位输入并同步音轨和字幕。
普通坏包和队列背压沿用原有处理，不直接触发后端切换。HEVC 有界恢复先重开实际生效的
硬件后端，耗尽后继续下一候选，通过缓存重放或定位恢复。Seek 不重置已尝试的候选，
重新打开视频才重新选择。回退不修改保存的 Auto 偏好；播放信息上报实际后端。

可通过 `TINY_HWDEC` 覆盖默认策略：`auto` 使用上述普通 Auto 行为，`auto-copy` 使用开发 Auto，
`off` 强制软件解码，
`vulkan` 或 `force-vulkan` 强制 Vulkan 硬解并禁止显式软解回退。
软件解码后的常见 YUV 帧仍经由 Vulkan/libplacebo 处理。

Linux 新增显式 `HardwareDecodeMode::ForceVaapi`（配置值 `force_vaapi`）：
`TINY_HWDEC=vaapi`、`force-vaapi` 或 `vaapi-copy` 选择 VA-API 回传路径。
`TINY_VAAPI_DEVICE` 可指定 DRM render node；未设置时由 FFmpeg 自动选择。
引擎选择具有 VA-API 硬件配置的解码器（包括绕过软件专用的 AV1/dav1d 默认解码器），
在解码 worker 接收输出时立即执行 `av_hwframe_transfer_data`，并复制帧属性及 side data。
异步队列只保留 CPU 帧，不长期占用 VA-API surface；surface 额外预留为 6 帧。
回传显式保留源像素格式，P010 不经过 8 位中间帧；不支持的格式明确报错。
转换结果沿用 `RawVideo`、libplacebo 与 BGRA 输出，不导入 DMA-BUF，也不共享解码设备。
强制格式协商不接受软件输出；HEVC 有界恢复保留 VA-API 后端，重开后直接重放，
无需等待 Vulkan 解码设备预热。强制模式失败时不回退软件，开发 Auto 将 VA-API 回传作为独立候选。
播放信息通过 `PlaybackVideoInfo::hardware_decode_backend` 上报解码器使用的后端，
面板中 VA-API 回传路径显示 `HW: vaapi-copy`，Vulkan 硬解显示 `HW: vulkan`。
`Display / Context: vulkan` 表示渲染后端，使用 VA-API 或软件解码时仍为 Vulkan。

Linux 性能路径 `HardwareDecodeMode::ForceVaapiVulkan`（配置值 `force_vaapi_vulkan`）
由 `TINY_HWDEC=vaapi-vulkan` 或“强制 VA-API（映射 Vulkan）”选择：

- 从 VA-API 设备派生 Vulkan 设备，关闭 multiplane 格式选择，使派生上下文的平面格式匹配 VA 导出的 DRM 分层图像。派生设备用于现有 libplacebo 设备导入及预热。
- 按源 `AVHWFramesContext` 身份缓存派生帧上下文；源池变化时替换，旧队列帧仍保留自身引用。两次 `av_hwframe_map(READ | DIRECT)` 显式执行 VA-API → DRM PRIME → Vulkan，经 DMA-BUF 导入 NV12/P010，不执行原始 YUV 帧下载。显式导出 DRM 避免 FFmpeg VA-API `map_from` 将 Vulkan 目标误派发到内存映射并返回 EINVAL。
- FFmpeg 处理 VA surface 完成同步、DMA-BUF modifier、外部内存及队列所有权，并在映射 AVFrame 中保留源 VA surface。渲染继续使用现有 FFmpeg 队列锁与 timeline semaphore 交接。直接映射接口及生命周期遵循 [FFmpeg hwcontext 文档](https://ffmpeg.org/doxygen/trunk/hwcontext_8h.html)。
- 映射的 VkImage 尺寸使用导入帧尺寸，usage 使用 READ 映射实际创建的 SAMPLED/TRANSFER_SRC 位，而不是派生池的默认能力。映射图像逐帧释放，禁用跨帧纹理包装缓存，并在解映射前完成 libplacebo 对图像和信号量的引用。
- 队列持有 GPU 帧及 VA surface；映射 AVFrame 附带引用计数租约，跨解码、准备、调度、呈现队列和渲染器统计实际存活映射。达到 8 帧后暂停送包，释放最后一个引用后恢复；控制命令不受此背压阻止。额外 surface 预留为 16，使用单解码线程，为 DPB、最多 4 个已送包和异步输出留出空间，兼容 NVIDIA VA-API 的 32 个解码 picture 限制；回传模式仍为 6。HEVC 重开保持 GPU 映射模式，并等待派生 Vulkan 设备预热后重放。
- 需要 `VK_KHR_external_memory_fd`、`VK_EXT_external_memory_dma_buf`、`VK_EXT_image_drm_format_modifier`、`VK_KHR_external_semaphore_fd`。设备派生、映射或直接渲染失败在强制模式下明确报错，不隐式回传内存；可在开发设置手动选择回传模式。Auto 按对应策略继续下一候选。

该路径消除的是解码到渲染之间的 YUV CPU 往返；输出仍经 BGRA 回读交给 GPUI，
不表示显示链路已实现端到端零拷贝。FFmpeg 的 VA surface 同步和当前呈现接口仍包含等待，
实际收益需在目标 GPU 上对比播放日志、CPU 占用和帧时间。

真机检查需要 FFmpeg 的 VA-API 支持、系统 VA-API 驱动、DRM render node 访问权限及
可用的 Vulkan 渲染设备：

```sh
vainfo --display drm --device /dev/dri/renderD128
TINY_HWDEC=vaapi TINY_VAAPI_DEVICE=/dev/dri/renderD128 \
  cargo run -p tiny-playback --locked --example headless -- /path/to/video.mkv
```

下述显式硬件测试检查 NV12/P010 回传、BGRA 渲染、暂停、前后 seek、恢复播放、EOF
及停止；需要 GPU，默认测试套件将其标为 ignored：

```sh
TINY_VAAPI_DEVICE=/dev/dri/renderD128 \
  cargo test -p tiny-playback --locked vaapi_playback -- --ignored --nocapture
```

GPU 映射模式的测试另外断言输出是 Vulkan GPU 帧，并覆盖相同播放操作及两种 demux 缓存策略：
硬件用例使用公开生成素材的二十秒无损循环版本，暂停 seek 按新会话与目标 PTS 验证，
恢复后四倍速排空至 EOF；默认无 GPU 的软件用例仍使用四秒原始素材。

```sh
TINY_HWDEC=vaapi-vulkan TINY_VAAPI_DEVICE=/dev/dri/renderD128 \
  cargo run --locked
TINY_VAAPI_DEVICE=/dev/dri/renderD128 \
  cargo test -p tiny-playback --locked vaapi_vulkan_playback -- --ignored --nocapture
```

信息面板应显示 `HW: vaapi-vulkan`；日志应出现 `created VA-API Vulkan DMA-BUF mapping device`、
`initialized direct VA-API to Vulkan frame mapping` 以及颜色转换日志的 `path: "vaapi-vulkan"`。
无 GPU 的默认测试覆盖直接映射标志、HDR/时间属性、排队帧持有源 surface 的引用、映射失败清理及帧池切换。

## Vulkan 共享队列兼容

FFmpeg 创建的 Vulkan 设备通过 `VulkanDecodeDevice` 保留 `queue_flags`，导入 libplacebo
时将其传给 graphics、compute、transfer 队列；回退到 graphics 的队列使用同一创建 flags。
这些是 `VkDeviceQueueCreateFlags`，与队列的 graphics/compute/transfer 能力位不同。
内部同步队列必须通过 `vkGetDeviceQueue2` 使用原始创建 flags 获取，否则可能得到空队列，
在首次 `vkQueueSubmit2` 时崩溃。

Linux 本地开发可先运行 `python3 -B scripts/prepare-linux.py`，在 `target/linux-native/`
准备与发布包相同的 libplacebo 修复快照，并生成本机 Cargo 环境配置；随后普通
`cargo run --locked` 和测试命令都会使用匹配的头文件及运行库。脚本不替换系统库，
也不覆盖其他工具生成的 Cargo 配置。重新准备前若已有自定义配置，需自行合并对应库路径。
预热及渲染完成日志包含 `succeeded`，任务完成或首帧入队均不代表图像渲染成功。

构建按头文件字段探测能力，兼容发行版回移补丁：FFmpeg 8.1 没有 `queue_flags`，使用零值；
libplacebo API 365 开始提供 `pl_vulkan_queue.flags`。旧 libplacebo 遇到非零创建 flags 时，
在调用设备导入前返回明确错误，由现有播放流程回退或报错，不进入不安全的队列提交。
Linux 预编译包使用同时包含取队列及导入 flags 修复的固定源码快照，Windows 旧库仍可编译。

无 GPU 的默认测试检查创建 flags 与能力位的区别、全部队列与 graphics 回退的 flags 保留，
以及旧 libplacebo 的导入前拒绝。以下显式测试需要 FFmpeg 9、支持内部同步队列的 GPU/驱动
和 libplacebo API 365+，实际创建 flags 必须包含内部同步位；零值会使测试失败：

```sh
cargo test -p tiny-playback --locked vulkan_import_renders_internally_synchronized_queues -- --ignored --nocapture
```

该测试上传 NV12/P010 图像、导入 FFmpeg 设备并渲染读回，无需实际视频解码；
它不能替代真实 GPU 上的桌面播放验收。

## 图像与应用适配

`VideoPresenter::render_if_needed` 返回 `Option<BgraImage>`。图像拥有紧密排列、
自上而下的 BGRA8 字节，alpha 为非预乘形式；构造时校验非零尺寸、溢出和字节长度。
像素尺寸可以小于视频源尺寸，布局与宽高比仍使用视频元数据。
HDR/Dolby Vision 映射和 Vulkan 渲染继续在引擎线程内完成，现有 CPU 回读路径不变。

`BgraImage` 不实现 `Clone`，应用通过 `into_bytes()` 接管像素分配。
`src/player/image_resources.rs` 将其包装为 GPUI `RenderImage`，视频适配不复制整帧。
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
