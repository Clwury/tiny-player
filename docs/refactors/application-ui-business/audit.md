# 应用 UI 业务层重构交付审计

> 本文保存 2026-09-29 的交付审计。其路径和检查数量属于当时的源码；当前架构见 [应用 UI 模块边界](../../application-ui-boundaries.md)，后续调整见 [迁移记录](progress.md)。

依据 [原始规范](spec.md)。2026-09-29 最新结论：**代码重构与自动检查完成；手动验收由用户执行**。用户明确要求“只需按 spec 文档完成重构修改，由我来手动测试验收”，因此不将代理运行手工 smoke 作为代码交付条件，不宣称已经证明全部视觉/媒体行为无差异。

模块边界、异步入口和关闭调用链见 [边界文档](../../application-ui-boundaries.md)，逐批迁移与历史门禁见 [迁移记录](progress.md)。原始 spec 未改写；用户暂存的 Cargo.toml/Cargo.lock 未修改或重新暂存。

## 要求与代码证据

| 规范 | 最终实现 / 自动证据 |
| --- | --- |
| §4.1、§5.1、§10.1 Shell | `ShellController` 拥有根路由、挂载 token 与订阅；`AppIntent` 在协调 Home/Playback 前验证挂载。TinyApp 保留窗口、overlay、global config、feature controller 和执行资源。服务器卡片实际绘制在 `server/view`，App 只组合 props 和 ID intent；Emby 调用在 gateway 适配器，不由 Shell 解析媒体 |
| §4.2、§5.1 Server | `ServerController` 私有拥有 catalog、选择/认证、计数、菜单和图标业务状态；重排通过 intent；表单使用独立 props/controller。同步 `CatalogChange` 保存成功后提交；凭据 snapshot/token 拒绝编辑/删除后的旧认证。认证、目录事务、侧栏切换与表单交互回归保留 |
| §4.3 ports | Server/Home/Playback gateway、AppPersistence 与 ImageRepository 归集协议/文件 IO；生产/fake 适配器均有测试。视频/字幕 BGRA 转换、缓存与两帧释放归 `player/presentation` 展示适配层；port 不返回 Window/Context/Entity |
| §4.4、§10.3 effects | 网络、分页、mutation、图片、播放 queue/reporting、计时器使用统一 slot/token/identity/handle。逐个生产 spawn/timer/result 入口已核对并记录；`generation` 只在 effects 基础类型。DropdownFocus 修复同 ID 重开菜单误收旧两帧回调；Editor caret 和通知 timer 的替换/释放也有回归 |
| §4.5、§10.4 persistence | App/Home 使用同一 PersistenceService/Coordinator，独立 dirty/snapshot/debounce/write token，Home 串行写入。设置关闭、主窗口释放、App/Home quit/release 与重复 flush 均有调用链和回归。失败保留 dirty，旧完成不清除新快照；JSON、版本/身份校验、路径、权限和原子替换沿用原适配器 |
| §5.2 Home owner | HomeController 聚合内容、用户数据覆盖、mutation 与 selector；HomeNavigation 拥有当前详情/历史；DetailBinding 只借用模型和展示资源。Feed/Search/Library/Favorites/Detail/Image 各自拥有请求状态，GPUI Entity、滚动、焦点、资源和执行句柄独立 |
| §5.2 Home read model | 详情 controls、卡片和操作投影在 model/controller 合并有效用户数据、版本与轨道优先级，组件接收小型 VM/callback。分页与详情读取借用模型，不为每张卡片克隆完整 Emby 条目；回滚、失效、快照与 warm dashboard 回归保留 |
| §5.3 Playback | Session 拥有 source/timeline/controls；queue/reporting 各有 controller；backend events 经 reducer 产生转换。页面执行 backend/window/展示适配，轨道菜单、剧集卡片、按钮、音量、缓存浮层接收 VM/props/callback；进度提示分离投影和绘制 |
| §5.3、§8 资源顺序 | `PlaybackBackendAdapter` 封装 driver/presenter；现有 ShutdownOrder 实际为 dependent/presenter 先于 owner/backend 释放。spec 同时要求保持原顺序且写作“presenter 后于 backend”，两者不一致：保留实际安全基线，不反转。两帧视频/字幕 atlas 释放及 adapter 生命周期回归通过 |
| §5.4 Settings | Controller/Intent/VM/PersistenceAdapter 覆盖主题、语言、解码、缓存、分类搜索。VM 借用逐字段 validation 并暴露 editable；验证投影变化驱动 view_changed，重复无效编辑不额外刷新父视图。Editor 保留原始草稿，精确已接受值留在 snapshot；不新增错误文案/样式或改变步进规则 |
| §5.4 通用 UI | 不直接依赖 CachedServer、EmbyClient、PlaybackPage、具体 IO 仓库或持久化服务。服务器图标 Asset 已移到 images adapter；picker 层传入要清除的 session/URL，控件不读取服务器目录。Editor、Dropdown、Notification 只拥有交互/展示状态 |
| §6 Phase 0–6 | 兼容事件/生命周期契约、迁移门禁和静态 trace 已记录；旧导航/详情 state、页面 runtime/request、Deref/DerefMut 和业务字段转发已移除。保留的 HomePage/PlaybackPage 是 GPUI 挂载边界，事件是跨 feature 契约，不是并存的旧业务实现 |
| §8 render IO/锁 | App render 的取消拖动路径只回滚预览，不保存；Home/Settings render 借用模型，目录解析在构造阶段，图像加载由后台 Asset/Repository 执行。直接 IO 反向检查仅在 Home cache 编码器找到文件读写，由持久化 adapter 调用。Playback render 的 report 提交现已改为无锁、同线程有界 staging，帧结束后的 foreground continuation 才接触 worker mailbox |
| §8 clone/notify/并发 | 卡片只构造展示字符串/ID，媒体、详情、源和字幕使用借用/Arc；轨道菜单仅在打开时持有小型选项和选择 payload。分页/搜索/布局/图片结果按接受与可见变化通知，隐藏 dashboard 保留独立缓存；latest/images 有并发限额，上报 pending progress 至多一项；新增 staging 每批最多一个 continuation，不持有页面 |
| §9 新状态生命周期 | Controller/执行资源定义和边界文档注明 owner、写入口、失效/释放、identity/scope/取消/错误去向。ReportDelivery 复用原 ReportingCommand token，不建立第二套请求代次；接受的 Stop 可越过页面释放，执行器丢弃 continuation 时由 staging Drop 先交付再关闭 transport |
| §10.5 engine 依赖 | 源码、manifest、Cargo 完整解析图（含 target/build/dev/间接依赖）检查通过；引擎无 GPUI 或应用反向依赖，两个公共 API 边界测试通过。未把应用状态或 GPUI 资源移入引擎 |
| §7、§10.6 验证 | 工作区自动测试、fmt、Clippy -D warnings、diff check、依赖检查全部通过。用户负责真实窗口/媒体、视觉对照与完整手动验收；这些不被自动测试结果冒充 |

## 本次明确缺口的关闭结果

| 编号 | 修正 |
| --- | --- |
| A1 | Home 卡片迁入纯 VM，合并用户数据后投影，避免完整条目深拷贝 |
| A2 | 封面读取/解码迁入 ImageRepository/Asset；卡片 Asset 保留应用缓存寿命，picker 预览按 session 显式清除 |
| A3 | Playback 轨道菜单/剧集卡片/控制栏组件使用 props、VM 与 callback |
| A4 | Editor caret 使用统一 token/handle，blur/drop 取消，保持 500ms 节奏 |
| A5 | **手动验收由用户接管**；临时原生工具/截图/日志移到 `/tmp/tiny-refactor-native-smoke/archive`，不纳入仓库修改 |
| A6 | Settings validation/editable 纳入 VM；新增两种模式的隔离、恢复、重复无效编辑、导航与关闭回归 |
| A7 | DropdownFocus 拒绝同 ID 重开前的旧两帧回调；关闭/释放回归通过 |
| A8 | 上报 mailbox 提交移出 render；共享 Pending 策略保持 Start/Stop 顺序与最新 Progress 合并，三个新测试覆盖不进入队列锁、页面释放、跨账号拒收和执行器 teardown |

## 最终自动门禁

- `cargo test --workspace --locked --quiet`：1478 engine、2 boundary、887 app、6 logging、3 doc，全部通过。
- `cargo clippy --workspace --locked --all-targets -- -D warnings`：通过。
- `cargo fmt --all -- --check`、`git diff --check`：通过。
- `python3 -B scripts/check_ui_boundaries.py`：437 个 Rust 源文件、engine manifest 与完整解析图通过。
- 检查器自测：9 项通过；包含直接/别名/平台/间接违规的反例。

临时输出为 `/tmp/tiny-refactor-final-workspace-tests.log`、`/tmp/tiny-refactor-final-clippy.log`、`/tmp/tiny-refactor-final-boundaries.log`。依赖源码 lint 是词法检查，不是完整 Rust 名称解析或宏展开器，不能替代编译、可见性约束和状态回归。


## 2026-09-30：目录收尾复核

目录评审中的六组建议已落地：纯层门禁与真实目录自测、Home feature 模型归属、业务视图与通用控件分离、共享媒体运行时绑定、Home 浏览装配与播放 port 拆分，以及命名/公共夹具整理。当前代码位置、接口与生命周期见 [维护中的架构说明](../../application-ui-boundaries.md)，逐项变更和最新门禁见 [目录收尾记录](progress.md)。

最新自动证据为 2379 项 Rust 测试、14 项检查器自测、464 个生产 Rust 源文件与完整依赖图检查，格式及严格 Clippy 通过。原 spec 内容、引擎实现、依赖清单和开始时的暂存区均保持不变。自动证据覆盖编译、状态、注入与交互回归；真实窗口视觉、原生播放与用户手动验收仍由用户执行。


## 2026-10-01：后续目录优化复核

后续评审建议已按顺序完成：交付文档与边界脚本纳入版本控制范围，纯层约束覆盖同名子目录并禁止 mod.rs，Server 图标选择器绘制迁至 feature view，Player 来源与上报 port 从挂载入口注入，Detail/Sidebar 命名统一，三个职责偏多的模块按业务/展示分组。状态所有者、公开页面构造和既有交互/释放契约保持。

最新证据为 2381 项 Rust 测试、17 项检查器自测、481 个生产 Rust 源文件及完整引擎依赖图；fmt、严格 Clippy 与 diff 检查通过。当前职责见 [架构说明](../../application-ui-boundaries.md)，逐项变更见 [迁移记录](progress.md)。手动验收仍由用户执行。
