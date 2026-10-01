# 应用 UI 业务层重构 Spec

**状态**：Draft
**范围**：`tiny-player` 应用层（`src/app`、`src/home`、`src/player`、`src/ui`）
**约束**：重构期间保持现有用户行为、Emby 请求语义、缓存格式、播放引擎边界和 GPUI 展示效果不变。本 Spec 本身不要求修改现有业务逻辑。

## 1. 目标与非目标

本次重构的目标是把应用层整理为可预测的单向状态流，使业务状态、异步副作用、GPUI 页面和通用控件可以分别测试、替换和演进。重构完成后应能回答以下问题：

- 某次用户操作对应哪个业务命令、修改了哪个状态、触发了哪些副作用。
- 异步响应属于哪个服务器、用户、页面或请求代次，以及为什么可以或不能提交。
- 页面渲染读取的是哪一份状态，窗口/弹窗/通知的生命周期由谁负责。
- 配置和首页快照何时被标记为脏、何时保存、关闭窗口或退出应用时如何完成 flush。

本次不包含以下内容：

- 不改变 Emby API、登录策略、媒体选择、播放控制、队列切换和上报时序。
- 不改变 `ServerCache`/首页快照的 JSON 兼容性、路径、权限或文件替换策略。
- 不把播放后端重新放回应用层；`tiny-playback` 继续是独立 crate，应用只通过公开接口接入。
- 不重做视觉设计、交互文案、窗口尺寸、主题或现有 GPUI 控件行为。
- 不以一次大提交替换所有测试；每个阶段都必须可以单独编译、测试和回滚。

## 2. 当前实现基线

### 2.1 根应用壳层

`src/app.rs` 的 `TinyApp` 目前同时持有：

- 页面路由：`Servers`、`Home(Entity<HomePage>)`、`Playback { page, return_to }`。
- 服务器列表、当前认证任务、服务器卡片拖拽/右键菜单、图标选择器。
- `ServerCache`、Emby client、项目级和服务器页通知。
- item count 请求状态、窗口尺寸观察、设置窗口句柄。
- 配置保存任务、保存错误重试状态和退出时清理。

`src/app/auth.rs`、`server_cache.rs`、`cache_save.rs` 等文件只是把 `TinyApp` 的方法拆到不同文件，所有权和状态仍集中在同一个实体中。`src/app/render.rs` 还负责窗口装饰、标题、页面挂载和服务器页渲染，因此窗口框架、路由和业务渲染相互可见。

### 2.2 首页与浏览业务

`src/home.rs` 已有 `HomePage` 外壳和 `HomeContent` 内部实体。`HomePage` 持有侧栏、服务器选择状态和 `Entity<HomeContent>`；`HomeContent` 持有：

- `HomeNavigation` 路由栈（Home、Favorites、Search、Library、Detail）。
- 首页快照、用户视图、继续观看、库分页、收藏、搜索、详情和播放来源。
- 多组 carousel、scroll handle、图片加载器和 resize/warmup 渲染缓存。
- 用户数据 revision、favorite rollback、请求集合、请求代次和服务器/用户身份校验。
- 通知队列、搜索编辑器、首页快照保存任务和若干延迟释放/刷新任务。

`src/home/data.rs`、`library.rs`、`detail/loading.rs`、`detail/actions.rs`、`search.rs` 等模块以 `impl HomeContent` 扩展同一个状态对象。当前异步请求统一采用 `background_spawn` 执行 IO，再用 `cx.spawn` 回到实体；响应通过 `WorkspaceIdentity`、`generation`、`user_data_revision` 和分页代次手工丢弃过期结果。

### 2.3 播放页面

`src/player/page.rs` 的 `PlaybackPage` 同时持有：

- FFmpeg backend/presenter 的 `ShutdownOrder` 生命周期。
- 帧、时间线、缓冲、进度拖拽、音量、倍速、全屏和窗口拖动状态。
- 播放队列、剧集列表、切集任务、轨道选择、字幕图像缓存。
- Emby 播放上下文、定时进度上报、停止上报完成状态和错误信息。

`backend_events.rs`、`session.rs`、`queue.rs`、`controls/*` 等模块同样通过 `impl PlaybackPage` 操作页面实体。页面对外发出 `PlaybackEvent::{VolumeChanged, Update, Back, Replace}`，由 `TinyApp` 更新配置、回写首页或替换播放实体。

### 2.4 UI 与状态混合

`src/ui` 已有可复用的 `Editor`、通知队列、滚动条、标题栏、设置控件、设置弹窗和服务器图标。当前设置弹窗仍直接读取/修改 `PlaybackCacheConfig`、主题和轨道语言，并通过 `SettingsChanged` 让 `TinyApp` 保存和应用配置。部分控件拥有自己的 `Entity`/`Task`/`EventEmitter`，但没有统一的“展示状态与业务命令”边界。

### 2.5 当前优点

- 关键异步路径已有身份、代次和请求集合保护，能处理服务器切换、编辑竞态和搜索过期响应。
- 关闭窗口、应用退出和实体释放路径已有保存 flush 与媒体图像延迟释放处理。
- 首页、详情、播放和设置已有大量行为测试；测试覆盖了导航、缓存、交互、保存、队列和播放恢复。
- `tiny-playback` 已与 GPUI 和应用业务隔离，当前边界应继续保持。

## 3. 需要解决的结构问题

### 3.1 状态所有权不清晰

同一业务状态在多个实体之间传递。例如服务器选择状态同时出现在 `TinyApp.selecting_server_id` 和 `HomePage.selecting_server_id`；当前页面、返回来源和播放事件由 `TinyApp`、`HomePage`、`PlaybackPage` 通过订阅关系共同维护。新增流程容易出现“写了一个副本但忘记同步另一个副本”的风险。

### 3.2 业务命令与 UI 回调耦合

页面事件直接调用 `TinyApp` 的方法；控件回调中混合了关闭菜单、请求网络、修改缓存、发通知和 `cx.notify()`。这使得同一业务动作难以在无 GPUI 的测试中复用，也使渲染代码需要知道过多的业务对象。

### 3.3 异步生命周期缺少统一抽象

当前已有多种代次：全局刷新代次、详情代次、搜索代次、分页代次、播放队列代次、定时隐藏代次和保存代次。它们语义相近但类型不同，取消通常通过递增数字或替换 `Task` 完成。继续增加功能会放大漏检过期响应、重复请求和忘记清理任务的风险。

### 3.4 读模型和写模型混合

`HomeContent` 同时保存 Emby 原始数据、乐观覆盖数据、渲染缓存、滚动位置、通知和 IO 状态。渲染函数因此需要理解加载状态、请求状态和缓存失效条件；业务更新也容易直接触发多个局部刷新。

### 3.5 持久化责任分散

配置保存属于 `TinyApp`，首页快照保存属于 `HomeContent`，播放进度通过独立 reporter 线程上报。各自逻辑合理，但缺少统一的“脏状态、保存请求、错误展示、关闭时 flush”协议。

## 4. 目标架构

目标架构采用四层和单向数据流。目录名称可以在实施时调整，但依赖方向必须保持。

```text
GPUI View / Component
        │  UserIntent
        ▼
Feature Controller / Reducer  ─────► Effect Runner ─────► Port / Adapter / Emby
        │        ▲                         │                         │
        │        └──── Domain Result ◄─────┘                         │
        ▼                                                            │
Feature State / Read Model  ◄───────────────────────────────────────┘
        │
        └──── ViewModel / Selector ───► GPUI render
```

### 4.1 App Shell

新增一个应用壳层概念（可从 `TinyApp` 逐步迁移），只负责：

- 创建 GPUI 窗口、主题和顶层资源。
- 持有当前根路由、全局配置、窗口级 overlay 和跨页面通知。
- 分发顶层 `AppIntent`，挂载/卸载 feature controller。
- 在退出、主窗口关闭和设置窗口关闭时触发统一的 persistence flush。

App Shell 不直接调用 Emby API，不解析媒体数据，不持有首页详情字段，也不渲染业务卡片。

建议的顶层路由模型：

```rust
enum AppRoute {
    Servers,
    Home(WorkspaceId),
    Playback(PlaybackSessionId),
}

struct OverlayState {
    add_server: Option<AddServerDialogModel>,
    server_menu: Option<ServerMenuModel>,
    icon_picker: Option<IconPickerModel>,
    settings: Option<SettingsWindowModel>,
}
```

路由和 overlay 分离，避免“页面枚举里隐含弹窗生命周期”。页面实体可以保留以满足 GPUI 生命周期，但实体引用应由 controller/store 持有，不再作为业务数据模型使用。

### 4.2 Feature State 与 Controller

每个 feature 使用以下结构：

```text
feature/
  model.rs       纯业务类型、状态和不变量
  intent.rs      用户意图
  command.rs     可执行业务命令
  effect.rs      异步副作用与结果
  reducer.rs     Result/Command 对状态的变更
  selectors.rs   给视图使用的只读 ViewModel
  view.rs        GPUI 组合和事件绑定
  adapter.rs     Emby、文件系统或 tiny-playback 适配
```

Controller 的职责是把 `Intent` 转为命令、调用 reducer、启动 effect，并将 effect 结果提交回同一个 feature state。View 只能发送 intent 和读取 selector，不直接写业务字段。

### 4.3 Ports 与适配器

将当前直接使用 `EmbyClient`、`storage::save_to`、`home::cache::*` 和播放后端的代码收敛到 ports：

- `ServerGateway`：公共系统信息、登录、item count、服务器图标元数据。
- `HomeGateway`：用户视图、继续观看、分页、详情、相似内容、媒体来源和用户数据操作。
- `PlaybackGateway`：媒体来源解析、队列切换、播放会话报告。
- `AppPersistence`：`ServerCache`、窗口状态、搜索历史和首页快照的读写。
- `ImageRepository`：服务器图标、首页图片、字幕/视频展示图像的缓存和回收。

生产环境使用现有 Emby/文件/GPUI 适配器；测试使用可控 fake。Port 返回业务结果，不返回 `Context<T>`、`Window`、GPUI element 或实体句柄。

### 4.4 统一 Effect 生命周期

用类型化 request key 替代散落的裸 `u64`：

```rust
struct RequestToken {
    scope: RequestScope,
    generation: u64,
    identity: WorkspaceIdentity,
}

enum RequestScope {
    ServerAuth,
    HomeSnapshot,
    UserViews,
    ResumeItems,
    Library { view_id: String },
    Detail { item_id: String },
    Search,
    PlaybackQueue,
    Persistence,
}
```

统一规则：

1. controller 启动请求时生成 token，并记录可取消的 effect handle。
2. result 必须通过 `token.is_current(state)` 才能进入 reducer。
3. 路由、服务器、用户、查询词或目标 item 变化时使对应 scope 失效。
4. 被替换的请求不再触发业务通知；真正的 IO 错误才进入 notification model。
5. 页面释放时取消 effect；无法取消的阻塞 IO 只允许提交到仍匹配的 token。

这套机制保留现有 identity/generation 的行为，只统一命名、校验和测试方式。

### 4.5 Persistence Coordinator

引入应用级 `PersistenceCoordinator`，统一管理：

- `DirtySet::{Settings, SearchHistory, Window, HomeSnapshot(WorkspaceId)}`。
- 每个 key 的 debounce timer、in-flight write、失败信息和最后一次有效快照。
- `schedule(key)`、`flush(key)`、`flush_all()` 三类操作。
- 关闭设置窗口、主窗口关闭、应用退出和实体释放时的幂等 flush。

现有 atomic temp file、权限设置、快照版本和失败重试行为必须由 adapter 保留；controller 只提交不可变 snapshot，不直接操作路径。

## 5. Feature 拆分规格

### 5.1 Server Feature

把 `TinyApp` 中的服务器业务抽成 `ServerFeatureState`：

- `servers: Vec<CachedServer>`、`selected_server`、`auto_start_server_id`。
- `selection: Idle | Authenticating { server_id, token } | Failed`。
- `reorder: Idle | Dragging | Preview(Vec<ServerId>)`。
- `server_menu`、图标 picker 和 item counts 的业务模型。

用户动作包括 `SelectServer`、`AddServer`、`EditServer`、`DeleteServer`、`ReorderPreview`、`CommitReorder`、`ToggleAutoStart`、`OpenIconPicker`。UI 只产生这些动作；登录结果、保存结果和 item count 结果作为 effect result 进入 reducer。

认证提交前必须保留当前的 server snapshot 校验：编辑或删除发生后，旧认证结果不能覆盖新凭据。缓存认证可复用的判定逻辑继续由 `CachedServer::can_reuse_auth` 定义。

### 5.2 Home Feature

将 `HomeContent` 拆成三个状态域：

1. **NavigationState**：`HomeRoot`、`HomeRoute`、detail history、当前选中 season/episode/source。
2. **ContentState**：首页 sections、library/favorites/search/detail 的业务数据和加载状态。
3. **PresentationState**：carousel offset、scroll handle、resize warmup、图片引用、菜单和 tooltip。

`SeriesDetailState` 保留现有选择和播放约束，但将请求状态（`SeriesDetailEffects`）和渲染状态（scroll/carousel/overlay）分开。`FavoritesState`、`PagedItemsState`、`SearchState` 作为可独立测试的子状态。

首页 read model 只暴露 selector 结果，例如 `HomeHeaderVm`、`SectionVm`、`LibraryGridVm`、`SeriesDetailVm`。selector 负责合并原始数据与 `user_data_overrides`、`VideoVersion` 和加载状态，view 不再直接拼装业务优先级。

保留以下现有不变量：

- `WorkspaceIdentity` 不匹配的结果被丢弃。
- `home_refresh_generation`、`detail_generation`、搜索和分页代次失效规则不变。
- 收藏和已播放操作的 optimistic update/rollback 行为不变。
- 首页快照只保存允许缓存的字段，跨服务器或用户不复用。
- 路由返回 Home 时可复用 warm dashboard，但数据通知仍会使其失效。

### 5.3 Playback Feature

保持 `PlaybackPage` 作为 GPUI 页面壳，但抽出 `PlaybackSessionState`：

- `PlaybackSourceState`：请求、媒体信息和选中轨道。
- `PlaybackTimelineState`：位置、时长、缓冲、暂停、结束、进度拖拽。
- `PlaybackQueueState`：队列、当前项、切换状态和代次。
- `PlaybackPresentationState`：帧、字幕图像、视口、全屏、音量指示器、窗口拖动。
- `PlaybackReportingState`：启动、周期进度、停止和完成状态。

`PlaybackPage` 只把 backend event 转成 `PlaybackIntent`/`PlaybackResult`，通过 selector 生成控制栏和视频视图所需的数据。`tiny-playback` 的 backend/presenter 仍由 `PlaybackBackendAdapter` 持有，且释放顺序继续是 presenter 后于 backend 的现有 `ShutdownOrder` 语义。

切集、替换页面和返回首页的事件契约保持现有 `PlaybackEvent` 语义；可以先包一层 controller，再逐步把 `TinyApp` 的订阅逻辑迁移过去。

### 5.4 Settings 与通用 UI

设置分为：

- `SettingsModel`：主题、语言、硬件解码、缓存配置和 UI 模式。
- `SettingsIntent`：选择、切换、编辑、搜索分类、关闭。
- `SettingsViewModel`：分类、标签、可编辑性、当前值和 validation state。
- `SettingsPersistenceAdapter`：把已验证配置提交到 `ServerCache` 和当前播放会话。

`Editor`、`DropdownState`、`NumberControl`、`NotificationQueue`、标题栏和滚动条继续作为纯 UI/交互组件。组件不得直接依赖 `CachedServer`、`EmbyClient` 或 `PlaybackPage`；业务页面通过小型 props/view model 和 callback 传入数据。

## 6. 分阶段迁移计划

### Phase 0：冻结契约与观测

- 建立现有路由、事件、保存时机、请求 token 和关键用户流程清单。
- 为 `HomeEvent`、`PlaybackEvent`、设置变更和保存结果补充 debug-only 结构化日志/trace id。
- 不移动业务代码，不改变输出；完成后可通过现有测试和手工流程建立基线。

### Phase 1：提取纯模型与 reducer

- 先提取 `HomeNavigation`、`PagedItemsState`、`SearchState`、`SeriesDetailState`、播放 timeline/queue 状态的纯操作。
- 保留现有实体字段，通过 wrapper 调用旧方法。
- 为每个 reducer 增加过期 token、乐观更新、rollback 和边界测试。

### Phase 2：统一 Effect 与 Persistence

- 引入 `RequestToken`、`EffectHandle` 和 `PersistenceCoordinator`。
- 先迁移搜索、分页、服务器认证和保存任务；保留旧事件名作为兼容层。
- 验证关闭窗口、应用退出、服务器切换和快速重复操作的行为。

### Phase 3：拆分 App Shell 与 Server Feature

- 将 `TinyApp` 的服务器状态迁入 feature store。
- `TinyApp` 保留 GPUI window、route、overlay 挂载和全局通知转发。
- 先迁移服务器页，再迁移设置窗口和图标 picker；确保 `HomePage` 的 entity 复用行为不变。

### Phase 4：拆分 Home Feature

- 依次迁移 Home root、Search、Library/Favorites、Detail，最后迁移 resize/warm dashboard 和图片缓存。
- `HomeContent` 先变成 facade，内部调用新 controller；测试通过后再移除旧 facade 字段。
- 每一步保持快照格式、请求参数、通知 key 和 route history。

### Phase 5：拆分 Playback Feature

- 先把 reporting、queue switch、backend event reducer 化，再处理 controls 和 presentation。
- 保持 `PlaybackEvent` 作为 app-facing compatibility event，待所有调用方迁移后再决定是否改名。
- 对播放起播、切轨、切集失败、停止上报和页面替换做长生命周期测试。

### Phase 6：收敛 UI API 与删除兼容层

- 所有业务组件改为接收 `ViewModel + Intent callback`。
- 删除重复状态副本、旧代次字段和 facade 中仅用于转发的方法。
- 更新模块边界文档和依赖检查，确保 UI 不反向依赖业务 adapter。

## 7. 测试与验收

### 7.1 必须保持的行为

- 服务器卡片选择、认证失败、编辑期间旧请求返回、删除和重排。
- 首页缓存优先、网络刷新、搜索取消/过期、库分页、收藏 rollback、详情返回栈。
- 播放起播、暂停/恢复、进度拖拽、音量持久化、切集失败恢复、字幕和停止上报。
- 设置窗口的自动保存、精度保持、配置应用、关闭 flush 和重启恢复。
- 主窗口尺寸保存、主题、窗口装饰、全屏和 modal/notification 叠层优先级。

### 7.2 新增测试分层

| 层 | 内容 | 依赖 |
| --- | --- | --- |
| Model/Reducer | 状态转移、不变量、过期 token、rollback | 无 GPUI、无网络 |
| Effect/Adapter | fake gateway、响应乱序、失败与取消 | Tokio/测试 executor 或最小 fake |
| Controller | intent 到 effect/result 的完整业务链 | fake gateway、内存 persistence |
| GPUI interaction | 现有点击、键盘、拖拽和布局测试 | `TestAppContext` |
| End-to-end smoke | 登录→首页→详情→播放→返回、设置重启恢复 | mock Emby 或录制响应 |

现有 colocated 测试不要求立即移动。迁移每个 feature 时，先让旧测试继续通过，再把纯业务断言复制到 reducer/controller 测试，最后删除重复的实体构造辅助函数。

### 7.3 阶段门禁

每个阶段合并前必须通过：

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --locked --all-targets -- -D warnings
git diff --check
```

涉及播放或窗口的阶段还必须保留对应手工 smoke 流程和无凭据日志；不得把真实服务器 URL、token 或密码写入测试输出。

## 8. 性能、并发和资源约束

- 渲染帧中不得执行网络、文件 IO 或阻塞锁等待。
- 现有图片、视频帧、字幕图像的延迟释放和 presenter/backend 释放顺序必须保留。
- selector 应避免在每一帧深拷贝完整 `UserItems`、详情和字幕数据；必要时使用 `Arc`/不可变 snapshot。
- 同一 request scope 的重复请求应可合并或取消；快速搜索和快速切换服务器不能积累无限任务。
- `cx.notify()` 只在可见 read model 变化时触发；后台状态变化不应导致不必要的整页重绘。
- 用 trace id 记录请求启动、丢弃、提交、保存 flush 和播放会话关闭，便于定位竞态而不暴露凭据。

## 9. 风险与回滚

主要风险是拆分实体时破坏 GPUI entity 生命周期、遗失旧响应校验、改变缓存保存时序、增加页面重绘，或错误释放播放图像/后端。每一阶段必须保留旧 facade 和兼容事件，允许通过 feature flag 或构造函数选择旧路径；发现行为差异时回退到上一阶段，不回退 `Cargo.lock`、用户配置或媒体缓存文件。

任何新状态字段都必须注明 owner、写入入口、失效条件和释放时机。任何新异步请求都必须注明 identity、scope、取消方式和错误通知 key；缺少其中一项不得进入实现。

## 10. 完成定义

重构完成需满足：

1. `TinyApp` 只承担 app shell、路由、overlay 和跨 feature 协调，不再直接拥有首页/播放业务明细。
2. Home、Server、Playback、Settings 的业务状态均有明确 owner，UI 通过 ViewModel 和 Intent 交互。
3. 所有异步结果都经过统一 token/identity 校验；没有裸 generation 语义重复实现。
4. 配置与首页快照由统一 persistence 协议调度，并保留现有保存格式和 flush 语义。
5. `tiny-playback` 仍不依赖 GPUI 或应用模块。
6. 现有工作区测试、Clippy、格式检查和关键手工流程全部通过，且无用户可见行为差异。
