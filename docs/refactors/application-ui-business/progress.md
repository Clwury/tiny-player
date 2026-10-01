# 应用 UI 业务层迁移记录

规范： [spec.md](spec.md)。
本记录保留迁移过程与各批证据，不缩减规范范围。**最终状态：spec 重构代码和自动门禁已完成；手动验收由用户执行。** 下文早期“待完成”是当时状态，最终结论以验收审计及末尾门禁为准。
逐项要求与当前缺口见 [验收审计](audit.md)。

用户最新明确范围：代理只完成 spec 重构修改及自动检查，手动测试验收由用户负责。下方历史中“人工 smoke 待完成”不再代表代理交付前置条件。

## Phase 0：冻结的契约

| 边界 | 当前契约与所有者 | 保持行为的回归证据 |
| --- | --- | --- |
| 根路由 | `TinyApp::Page` 持有 Servers、Home entity、Playback entity 及原 Home entity；返回复用原 entity | `app::auth::tests`、`app::server_card::tests::sidebar` |
| Home 路由 | `HomeController` 聚合工作区业务；其 `HomeNavigation` 拥有非空根路由栈和全部 DetailController；展示资源按 DetailId 独立保存；切 root 清历史，返回恢复选择与滚动位置 | `home::model::navigation::tests`、`home::detail::navigation::tests` |
| HomeEvent | BackToServers、SwitchServer、AddServer、ReorderServer、SectionChanged、TitleChanged、SearchHistoryChanged、OpenSettings、OpenPlayback | `app/auth.rs` 订阅；Home/Sidebar interaction tests |
| PlaybackEvent | VolumeChanged 归一化后延迟保存；Update 更新保留的首页；Back 更新并复用首页；Replace 更新首页再替换播放 entity | `app/auth.rs`、`player::page::session::tests`、`player::page::mouse_tests` |
| 设置变更 | `SettingsChanged` 读取精确值、更新主题/语言、应用当前播放配置、350ms debounce 保存；关闭不撤销编辑 | `app::cache_save::tests`、`ui::playback_settings_dialog::interaction_tests`、`ui::user_settings_dialog::interaction_tests` |
| 服务器认证 | 缓存复用由 `CachedServer::can_reuse_auth` 决定；同目标合并；编辑/删除后的旧 snapshot 不提交；当前侧栏项取消切换 | `app::auth::tests`、`app::server_cache::tests` |
| 配置保存 | `storage` 保持 JSON/path/private permission/atomic replacement；失败保留 dirty，下一变更重试；设置窗口关闭、主窗口释放、quit flush | `app::cache_save::tests`、`storage::tests` |
| 首页快照 | server/user 身份与版本校验；缓存优先；收藏请求完成后才保存；串行 writes；释放 detach final flush，quit await | `home::cache::tests`、`home::favorites::integration_tests`、`home::track_preferences_tests` |
| 播放上报 | Prepared→Started→Closed；10秒 progress，队列合并 progress，Stopped 优先且仅一次；完成标记供首页刷新等待 | `player::page::session::tests` |
| 展示资源 | presenter/backend 使用现有 `ShutdownOrder`；图片、字幕保留延迟释放；warm dashboard 数据失效与 resize 路径不改 | `player::backend::lifetime::tests`、`home::render::resize_tests` |

### 当前请求保护清单

| 路径 | 身份 / scope | 失效与释放 / 错误 |
| --- | --- | --- |
| 服务器选择 | `RequestToken(ServerAuth)` + endpoint/username/password/needs_auth_refresh snapshot | 替换 EffectHandle；编辑/删除校验；登录失败通知 |
| item counts / 图标 / 添加编辑提交 | ServerCounts(server_id) / ServerIcon / ServerSave token；count 保留凭据、token、用户快照校验 | feature controller 接收结果；删除、编辑、关闭/替换后丢弃；统一可取消 handle；图标/保存失败保留原目录并可重试 |
| 首页 snapshot/views/resume/latest | `FeedController` 的 HomeSnapshot/UserViews/ResumeItems/LatestItems(view_id) token | refresh 失效；普通读取取消句柄，旧 Latest 只归还最多 4 个阻塞 IO 名额；原 Home 通知 key |
| 库/收藏分页 | `RequestToken(Library(view_id)/Favorites(item_type))` + start index + user data revision | sort、dirty、refresh 使 token 失效并取消句柄；initial/load-more/refresh 独立通知 key |
| 搜索 | `RequestToken(Search)` + query + start index + user data revision | 输入变化/清空取消句柄；initial/load-more 通知 key |
| 详情/季/相似/媒体来源 | DetailController 的 Detail(item_id, resource) token；分集额外校验 season + user data revision | 独立可取消 handle；进入历史/切 root 立即失效，返回恢复未完成加载；接受结果后才合并用户数据/分配图片/通知 |
| 详情激活/起播解析 | DetailActivation(item_id) / DetailPlayback(item_id) token；PlayedRequest 捕获发起详情的激活 token | 离开详情失效，返回创建新激活 token；切单集/版本取消解析，旧成功/错误不打开播放器或消耗字幕草稿；Home 不再保存 detail_generation |
| 收藏切换 | FavoriteMutation(item_id) token；FavoriteActions 唯一 pending/rollback owner | 乐观覆盖与分类移除；失败恢复；重复/跨 workspace 结果不修改数据或保存；释放取消 handle |
| 已播放 / 继续观看操作 | PlayedMutation(item_id) / ResumeMutation(item_id) token；纯 controller 接受结果 | 已播放串行、不同继续观看条目可并行；重复/跨 workspace 结果不提交；原通知、revision 和保存顺序保持；整剧结果校验 DetailActivation token 与季选择 |
| 首页/剧集封面 | ImageController 的 ItemImage(key) token + workspace identity | 每个 key 去重、限制并发/重试；释放取消 continuation；旧尝试/旧 owner/跨账号结果不更新路径、失败次数或通知；同 workspace 变换 route 保留完成缓存 |
| 切集 | QueueController 拥有 PlaybackQueue scope / workspace identity / opaque owner token | EffectHandle 随手动返回、后端失败和页面释放取消；只接受当前结果；失败保留 playback-queue-switch-error 展示，手动恢复暂停状态、自动失败回写结束状态 |
| 首页缓存 followup / 播放返回刷新 | HomeCachedImages/HomeNetworkRefresh/HomePlaybackRefresh token + handle | refresh/替换取消 timer，释放取消；仅当前成功的停止上报触发数据刷新 |
| Home resize / dashboard warmup | HomeLayoutController 的 HomeResize / HomeDashboardWarmup token + identity | 一次拖动一个可取消 timer；新 bounds 延长 120ms 静止期限；新 resize 失效旧两帧 warmup，释放取消投递；布局等待无业务错误通知 |
| 播放上报 | ReportingController 的 PlaybackReportTimer 与每个报告的 PlaybackReportStart/Progress/Stop token | 周期 handle 在关闭/释放取消；已接受的报告由绑定账号的串行 worker 完成，停止回执随页面释放后继续；错误只记录静态日志并设置 Failed，无弹窗 key |
| 暂停缓存轮询 | PlaybackSessionController 的 PlaybackBackendPoll token + identity | 250ms 单任务；结束/失败/返回/释放取消，回调再次校验 backend/error/cache 状态；无业务错误通知 |
| 播放展示与本地 UI timer | 四种 PresentationTimer、EditorCaretBlink、DropdownFocus typed scope/slot | 替换/关闭失效，弱实体与统一 handle 管理等待；必要的两帧资源回收按框架寿命保留 |
| 保存 | 应用级 PersistenceCoordinator + Persistence token；Settings/Window/SearchHistory 合并配置资源，HomeSnapshot 按 workspace 隔离 | 每个资源一个 debounce handle；首页串行 worker；close/release/quit flush；失败保留脏快照，失效的收藏快照等待 owner 重新提交 |

### 追踪约束

`src/observability.rs` 提供 debug-only `tiny_player::flow` 日志，使用进程内 trace ID、静态 operation/phase。禁止传入 URL、查询词、服务器名、用户 ID、认证信息、请求或错误的 Debug 输出。兼容事件在 app 订阅处记录，设置和快照写入记录开始/成功/失败。统一 effect 引入后继续沿用同一追踪接口。

### 必须执行的完整 smoke（首批已执行，尚未全部覆盖）

1. mock Emby 登录→首页缓存/刷新→库分页→详情→播放→返回原详情，重复切换服务器并使旧响应晚到。
2. 收藏/已播放成功与失败回滚；搜索输入变化、清空、翻页和晚到结果。
3. 播放暂停/恢复/seek/轨道/字幕/音量、切集失败恢复、关闭后停止上报与资源释放。
4. 设置两种模式自动保存、原有高精度值保留、关闭窗口 flush、重启恢复。
5. 主窗口 resize、主题、系统/自绘装饰、全屏、modal/通知叠层与图片延迟释放。

smoke 只使用 mock 或录制的脱敏数据；日志仅保留静态事件名与 trace ID。自动 GPUI 测试不能替代真实窗口与媒体 smoke。用户已接管所有手动流程；临时原生工具与产物已移出仓库。

## 阶段与最终交付状态

| 阶段 | 最终状态 |
| --- | --- |
| 0 冻结与观测 | 事件、保存、资源契约和静态 trace 完成；手动流程由用户接管 |
| 1 纯模型/reducer | Home 导航/分页/详情、Server、Settings、Playback session/source/timeline/queue/reporting 已接入 |
| 2 Effect/Persistence | typed token/identity/handle 与统一保存协调完成；生产入口与 close/release/quit 已核对 |
| 3 Shell/Server/Settings | Shell 路由与挂载校验、Server 目录事务和 VM/intent、Settings 验证读模型完成 |
| 4 Home | 导航/内容/展示分离，controller 私有拥有业务状态，selector 合并优先级，卡片使用小型 VM |
| 5 Playback | session/queue/reporting/backend/presentation 分离，组件接收 VM/callback，上报队列提交移出 render |
| 6 UI API/清理 | 旧状态与重复代次移除，图像 IO 归 adapter；依赖检查和边界文档完成 |

最终自动门禁覆盖 1478 个引擎测试、2 个边界集成测试、887 个应用测试、6 个日志测试、3 个文档测试和 9 个检查器自测。手动验收由用户负责，不宣称自动门禁已证明全部视觉与原生媒体行为。用户暂存 Cargo.toml/Cargo.lock、原始 spec 和用户配置没有被重置或覆盖。

## 2026-09-29：首批模型提取与自动验证

- `home/model` 现在持有原导航栈、分页状态、搜索状态及成功/失败/过期响应的转换；旧 Home 页面继续作为 facade。滚动句柄、网格列数和首次聚焦标记归 `home/presentation`，由各 library/favorite/search 展示对象持有，不复制业务状态。
- 搜索保持查询参数、过滤规则、原始分页游标、去重顺序、历史行为和通知 key。workspace 校验及 user data revision 合并仍在原 effect 提交入口；统一 token 和可取消 handle 尚未迁移。新测试覆盖晚到成功/错误、重复请求/响应、错误分页游标、失败重试和过滤后的原始数量。
- `player/model` 持有 timeline、时间校验与 queue switch 状态。暂停/缓存/位置/时长/restart reducer 已由 backend event 路径调用；进度条坐标、悬停和缓存弹窗移至页面展示状态，poll timer 的调度标记仍由页面持有。切集失败的恢复意图由 reducer 返回，页面继续按原顺序调用 backend、reporter 和 `PlaybackEvent`。
- 原页面、设置、滚动、鼠标、缓存、收藏 rollback、字幕、轨道与生命周期测试保留；新增 12 个测试。引擎源码、依赖边界、缓存格式与保存适配器未改。

首批模型提取时的验证记录（后续源码验证见下文）：

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 通过：1478 个引擎单元测试、2 个引擎边界集成测试、646 个应用测试、6 个日志测试、3 个文档测试 |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过 |
| `git diff --check` | 通过 |

测试在沙箱内绑定本机 mock HTTP 端口曾返回 `PermissionDenied`，随后通过已授权的沙箱外测试重跑确认通过，未跳过失败项。临时完整输出位于 `/tmp/tiny-refactor-phase0-tests.log`、`/tmp/tiny-refactor-phase1-tests.log` 和对应 `*-clippy.log`，不是长期验收产物；后续源码变更后必须重新验证。

首批结束时尚待提取详情选择、播放结束/失败和完整 queue 状态；这些项的后续实现见下文。统一 Effect/Persistence、Server/Settings/Shell、Home、Playback、UI API 和兼容层删除仍按原规范推进，不将模型提取视作最终架构完成。

## 2026-09-29：详情模型与首批统一请求生命周期

- `home/model/detail.rs` 接管详情数据、请求加载状态及 season/episode/source 选择规则。`DetailChange` 返回选择变化/失效/需要展示的单集等领域结果；`home/detail/state.rs` 的临时 facade 将结果应用到独立 `SeriesDetailPresentation` 的菜单、滚动和 carousel。全局轨道偏好查询留在 facade，纯模型不接收 GPUI context。原详情模型测试整体保留，新增选择变化和季请求失效测试。
- `player/model/timeline.rs` 接管结束位置、结束和失败转换，页面保持原 reporter、通知与资源释放顺序。`player/model/queue.rs` 接管完整队列和条目；标题使用 `Arc<str>`，在页面边界转为 GPUI 字符串，避免每帧复制整个条目。新增终止位置优先级和失败清理测试。
- `effects.rs` 是唯一的新请求生命周期实现：`WorkspaceIdentity`、类型化 `RequestScope`、`RequestToken`、不可克隆的 `RequestSlot`、`EffectHandle<H>`。token 校验 owner/scope/account/代次和活动状态，结果只提交一次；重新创建相同 scope 的 controller 也不能接受旧 token。workspace 的 Debug 输出隐藏身份字段，flow 日志只记录静态 operation/phase 和 trace ID。
- 搜索、媒体库和收藏分页已删除原裸 generation，改为由各自纯模型持有 `RequestSlot`。保留请求参数、原始游标、过滤/去重规则、user data revision、失败 checkpoint、rollback 和通知 key。页面持有单个 search 句柄、每个 library/category 一个句柄，替换、排序、dirty 和释放按既有刷新边界取消任务；阻塞 IO 即使完成也必须再校验 token。
- `HomeGateway` 和 `EmbyHomeGateway` 已承接 search/user-items 请求，其他 Home API 仍待迁入。fake gateway 测试覆盖正常参数、失败和乱序；GPUI 测试覆盖输入变化取消实际 continuation 和跨 workspace 数据/错误丢弃。分页测试增加跨库/分类 token 隔离以及同游标重试拒绝旧错误。
- 服务器认证也使用 `ServerAuth` token 和统一 handle，同时保留 `CachedServer::can_reuse_auth` 与 endpoint/username/password/needs_auth_refresh snapshot 校验。旧侧栏、编辑竞态测试继续执行，新增取消后重新选择同一服务器的晚到错误测试。服务器状态目前仍由 TinyApp 持有，Server feature 提取尚未完成。

本批新增 14 个测试。最新源码验证：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、660 个应用测试、6 个日志测试和 3 个文档测试全部通过；`/tmp/tiny-refactor-auth-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-auth-clippy.log` |
| `git diff --check` | 通过 |

中间模型、搜索与分页批次也分别运行了全量测试/Clippy，输出在 `/tmp/tiny-refactor-models-*`、`/tmp/tiny-refactor-search-effects-*` 和 `/tmp/tiny-refactor-paging-*`。测试仍通过既有沙箱外授权运行 mock HTTP。未改动用户已暂存的 Cargo.toml/Cargo.lock。

下一步优先完成 Phase 2 的 `PersistenceCoordinator` 与配置/首页快照 adapter，随后继续迁移剩余请求（首页、详情、用户数据操作、图像、播放队列及定时任务）。仍需完整 Server/Settings/Home/Playback controller、Intent/Selector 边界，删除 `SeriesDetailState` 的 Deref/DerefMut 与转发 facade、重复选择状态和旧代次；最后完成依赖检查、完整 smoke 和最终六项验收。当前不能据自动测试通过宣告整个重构完成。


## 2026-09-29：应用级持久化协调器

- `persistence/model.rs` 是无 GPUI/IO 的 `PersistenceCoordinator`：`DirtyKey::{Settings, SearchHistory, Window, HomeSnapshot(identity)}`、不可变最新快照、最后成功快照、失败信息、debounce deadline、统一 `Persistence` timer/write token 和 in-flight 状态。配置的三个 dirty reason 共用同一个原子文件资源，避免相互覆盖。重复 flush 不重复写同一份快照；旧结果不能清除更新后的脏状态。
- `persistence.rs` 的 `PersistenceService` 在 GPUI App global 中保存唯一实例，由 TinyApp 与所有 Home workspace 共享。每个资源只有一个可取消 debounce handle；配置继续在前台以 350ms 合并保存，首页 450ms 后通过单一后台 worker 串行写入。IO 期间不持状态锁，render 不读取协调器；后台完成不触发页面 notify。
- `AppPersistence` 与 `FilePersistence` 接管配置启动读取、首页读取和所有配置/首页写入。生产 adapter 委托原 `storage`/`home::cache`，后者只调整模块可见性，JSON、版本、路径、权限与临时文件替换实现均保持。控制层只提交完整不可变 snapshot。
- 移除 TinyApp 的四个保存调度字段，以及 HomeContent 的保存 pending、generation、task。搜索历史和窗口尺寸明确标记各自 dirty reason；音量/设置/服务器元数据仍使用同一配置文件。设置关闭、主窗口 release、app quit 调用统一 flush；Home release/quit 提交最后快照并等待同一个 drain barrier，页面不再拥有独立写队列。
- 收藏/已播放等路径使 pending Home 快照失效时，协调器保留 dirty 但暂停 timer 和全局 flush，直到 owner 提交已确认或已回滚的数据。关闭设置窗口不会提前保存悬而未决的收藏数据。quit 的最后快照直接进入 worker，不创建 foreground timer。
- 立即保存的服务器添加/编辑/删除仍是事务式行为：成功替代旧 debounce 快照；失败恢复此前 pending 快照与 timer，避免失败编辑稍后自动写回。通知继续由原 app notification 入口展示；首页保存仅保留安全的静态 saved/failed trace。失败快照保留，下一变更或显式 flush 可重试，无后台无限重试。

新增 13 个测试：6 个纯协调器转换，5 个内存 adapter/runner 测试，2 个真实 GPUI quit hook 测试；原配置关闭/精度/重启、首页轨道保存、收藏 rollback、文件格式/权限等测试全部保留。涵盖混合 dirty 合并、保存中编辑、重复/跨账户/晚到结果、失败快照及重试、事务回滚 timer、悬置快照与统一关闭、quit 最后状态与等待保存。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、673 个应用测试、6 个日志测试、3 个文档测试全部通过；`/tmp/tiny-refactor-persistence-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-persistence-clippy.log` |
| `git diff --check` | 通过 |

本批未改动引擎、依赖或已暂存的 Cargo 文件。统一 persistence 已实际接管，但这不代表整体架构验收完成。下一步迁移 Home 首页/详情/用户数据与播放等剩余异步 scope，继续拆分 Server/Settings/Shell、Home、Playback controller 与 VM/Intent，删除临时 facade 和测试转发方法。所有关键真实窗口/媒体 smoke 仍未执行，最终完成定义保持未达成。


## 2026-09-29：首页 FeedController 与请求生命周期

- `home/feed` 接管首页纯数据所有权、intent→request→result→reducer 链：缓存加载、用户视图、继续观看、Latest 排队与过滤、失败保留旧数据、重复请求合并、刷新和播放返回后的继续观看刷新。HomeContent 不再保存这些业务字段及 `HomeEffects`/`home_refresh_generation`，只持有 controller、effect handles 和展示对象。其他 Home 子域的 user-data reducer 暂时访问 `feed.state`，完整 feature 边界仍待后续收敛。
- `HomeSnapshot`、`UserViews`、`ResumeItems`、`LatestItems(view_id)` 使用统一 token；controller 在所有数据提交前校验 workspace/scope/owner/current。过期成功或失败都不会进入图片处理、user-data override 合并、保存和通知。每个当前结果只提交一次。
- Latest 保留原最多 4 个阻塞 IO 的限制。refresh 使旧 scope 失效并清除待发队列；已经运行的旧任务保留有限的完成句柄，只能归还并发名额。旧成功/错误和重复完成不能清除新请求的 loading、释放别的请求名额或发通知；workspace release 取消所有 continuation。这一规则有独立的纯 controller 测试。
- `HomeGateway`/`EmbyHomeGateway` 新增 user-views、resume、latest；`feed/effect.rs` 使用不可变 server/port snapshot，Latest 原 view ID、item types、limit=30 保持。库类型选择与支持规则移至纯 `home/model/library.rs`，原库相关测试不删改断言。
- 首页展示通过借用的 `HomeFeedVm`、`LatestRowVm` 读取可见性、空状态、缺失剧集提示和数据，不在帧内克隆整个响应。Latest carousel 独立归 HomeContent 的展示 map；刷新、resize 和返回同步原 offset 规则，原布局、菜单和 carousel 交互测试保留。
- 缓存命中后的 16ms 图片/网络 followup 改为 HomeCachedImages/HomeNetworkRefresh token 和可取消 handle。播放返回等待停止上报的 250ms/160 次轮询改为 HomePlaybackRefresh token/handle，删除裸 `playback_refresh_generation`，保留只有 Succeeded 才刷新以及同一 resume load 合并的规则。

新增 9 个测试：6 个纯 controller/fake gateway/内存 persistence 流程，3 个 GPUI 提交与实际 timer 取消测试。覆盖缓存优先、失败保留缓存、endpoint 参数、过滤、4 个请求上限、刷新后旧请求归还名额、重复/跨 workspace/重试旧结果、播放刷新合并、hydration 不覆盖已加载数据、晚到错误不通知、乐观 user data revision 和取消延迟网络启动。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、682 个应用测试、6 个日志测试、3 个文档测试全部通过；`/tmp/tiny-refactor-feed-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-feed-clippy.log` |
| `git diff --check` | 通过 |

测试过程中修复了新增 fixture 缺少必填 Name 和结果枚举尺寸问题，之后按最终源码重新跑完全部门禁。引擎和用户已暂存依赖变更保持。下一步仍需 Server/Settings/Shell 的实际所有权拆分，Home 搜索/分页/详情/用户数据/图片完整 controller 与剩余 token、Playback feature、UI Intent 边界及兼容层删除。真实窗口和媒体 smoke 未执行；本批自动测试不能证明最终六项完成定义。


## 2026-09-29：Server controller、图标生命周期与独立服务器页 view

- `server/feature` 现在拥有唯一可写服务器目录、自动启动、当前 workspace/认证选择、计数及请求、重排预览、菜单目标、图标搜索/选择/错误和提交请求。`GlobalConfig` 只保留应用级配置；保存时与 `ServerCatalog` 组合成原 `ServerCache`。移除 TinyApp 中旧服务器列表、认证与计数字段。新增完整 JSON round-trip 测试验证拆分与组合没有改变文档内容，包括窗口与数值精度。
- 目录的去重、添加、按 ID 编辑、删除规则从 storage 移到纯 catalog，storage 的加载归一化仍调用同一算法。添加/编辑/删除生成候选目录，只有统一 persistence 立即保存成功才提交；失败保留旧目录。编辑继续合并最新计数及原有目标冲突元数据，自动启动 ID 重映射规则保持。
- `ServerIntent`/`ServerCommand` 接管选择、取消、自动启动、拖拽预览/提交、菜单和图标动作。认证复用、重复请求合并、当前 workspace 取消切换、编辑或删除后拒绝旧凭据的规则保持。`ServerGateway` 提供公共元数据、认证、计数、图标匹配；HTTP 及原 `CachedServer::can_reuse_auth` 策略未改。
- 计数使用每服务器 `ServerCounts` token，添加/编辑使用单个 `ServerSave` token，图标使用 `ServerIcon` token。关闭或替换弹窗取消 continuation；阻塞 IO 晚到仍须通过 scope/account/current 校验，重复结果只提交一次。图标下载完成后合并最新目录再持久化，失败可重试；旧成功、旧错误和旧保存回执均不能改变新 picker。
- 图标 picker 的焦点、滚动、编辑器、预览 session UUID 与释放继续归 GPUI 展示。业务状态归 controller，查询只在输入变化时计算，匹配索引以 Arc 共享给 uniform list。UUID 仅用于预览图片生命周期，不再承担业务请求校验。`ImageRepository`/`FileImageRepository` 已接管服务器图标与预览 IO；既有下载限制、公共请求头、缓存 key、原子替换、解码和 asset 回收行为保持。首页和播放图像尚未迁入该 port。
- 服务器网格、卡片、菜单和拖拽预览移至 `server/view`，不依赖 TinyApp、CachedServer 或 EmbyClient。它们只接受小型 ServerCardVm/ServerMenuVm 和 ID 动作回调；卡片渲染不再克隆完整服务器凭据记录。TinyApp 只组装展示资源并在动作到达时重新解析 ID。重排动画和焦点规则保持，原 GPUI 卡片/侧栏/菜单/图标测试继续运行；两个已无调用方的点击转发方法已删除。

本批较 Feed 阶段新增 14 个测试：配置 JSON 组合、纯 controller 认证/取消/计数/重排/目录候选/菜单、fake gateway 与 ImageRepository、图标失败重试及过期结果。原保存失败事务、关闭弹窗、搜索、布局、焦点恢复、预览释放、窗口叠层和 Home entity 复用测试均保留。服务器图标 IO 测试随实现迁到 `images/server_icons/tests.rs`，断言未放宽。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、696 个应用测试、6 个日志测试、3 个文档测试全部通过；`/tmp/tiny-refactor-server-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-server-clippy.log` |
| `git diff --check` | 通过 |

中途修复了测试模块位置、结果枚举尺寸、SharedString 转换及迁移后旧字段引用，最后按当前源码重新执行完整测试与 Clippy。引擎及用户已暂存的 Cargo.toml/Cargo.lock 保持不变。此阶段仍不是 Phase 3 全部完成：Settings 业务模型、Shell 中的 effect runner/弹窗桥接、Home 侧栏服务器展示快照还需继续收敛。Home/Playback 的完整 controller、其余统一请求作用域、全部 ImageRepository 和兼容层清理仍待推进。真实窗口与媒体 smoke 尚未执行，最终六项完成定义仍未达成。


## 2026-09-29：两种设置界面的纯 controller 与精确值提交

- `settings/controller.rs` 持有当前设置窗口唯一可编辑的 SettingsSnapshot、模式、分类、搜索、字段校验状态和关闭状态。两种设置页面不再拥有 color theme、语言、缓存配置及各开关的业务副本；开发设置原 base_config 与独立开关/模式字段已合并。编辑器、dropdown、滚动、焦点和路径展示继续由 GPUI 界面持有。
- `SettingsIntent` 接管主题、语言、硬解、容量档位、缓存模式/清理、开关、数值编辑、分类、搜索和关闭。`SettingsVm` 借用当前配置，并用不含控件的 SettingDescriptor 执行分类/多词搜索筛选；视图只读 selector 并发送 intent。既有 UI 模式环境变量解析、分类顺序、标签、步进器和布局保持。
- 整数/秒数/GiB 输入解释迁入纯模型。无效、不完整、溢出或会被 normalization 改写的输入不会进入保存 snapshot；依赖字段仍按原规范联动。重新输入当前有效值只清除校验状态，不触发额外保存。未编辑值保留原始字节余数、浮点精度和路径，不从显示用的舍入文本回读配置。MemoryBudget 算法及原四个测试整体迁入 settings。
- 设置变更返回 persist/view/theme/language 结果。GPUI 适配器先应用主题或语言，再发原 SettingsChanged；主题应用失败时，controller 接收实际回退主题后再提交，保持原行为。SettingsPersistenceAdapter 把一个完整、已验证 snapshot 应用到 GlobalConfig，应用中的播放配置和统一 debounce/close/quit flush 路径保持。关闭只终止后续编辑，不撤销已提交变更。

本批新增 7 个纯 controller/selector/adapter 测试，覆盖两种模式的精度、无效输入及重试、联动归一化、分类/搜索、关闭、容量与全局配置隔离、主题回退。现有设置 GPUI 搜索、布局、键盘、自动保存、窗口关闭、精确值和重启恢复测试全部执行。原 getters 目前仅在测试编译中保留，生产提交统一使用 snapshot；后续 UI API 收敛时继续清理测试 facade。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、703 个应用测试、6 个日志测试、3 个文档测试全部通过；`/tmp/tiny-refactor-settings-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-settings-clippy.log` |
| `git diff --check` | 通过 |

最新校验已包含主题回退结果处理和搜索 selector。此前构建中发现的旧字段/测试导入引用均已修正，无跳过测试。用户已暂存 Cargo 文件、引擎实现和缓存文件协议保持。Server/Settings 已完成上述所有权迁移，但 Shell 的 effect/overlay 桥接、Home/Playback 完整 controller、剩余统一请求、图像生命周期、UI API 和兼容层删除仍未全部落地。真实窗口/媒体 smoke 未执行；目标继续保持进行中，不能据自动门禁通过宣告最终完成。

## 2026-09-29：Search 与 Library controller

- SearchController 成为查询、历史和分页状态的唯一写入入口。输入变化、提交、清除历史和加载更多通过 SearchIntent 执行；请求携带起始时的 user-data revision，过期结果在返回图片、通知和用户数据副作用前被拒绝。GPUI 页面保留编辑器、焦点、滚动和取消句柄，渲染借用 SearchVm。
- 每个媒体库由 LibraryController 拥有 view id、内容类型、排序、分页和请求作用域。LibraryState 只组合 controller 与菜单、滚动等 GPUI 资源。Open/SortBy/SortOrder/LoadMore 统一生成请求，移除了未使用的重复库标题。查询参数、60 项分页、原始响应计数、过滤/去重顺序、初次失败及刷新失败的图片处理、通知 key 保持原行为。
- 新增 4 个搜索 controller 测试、4 个库 controller 测试、1 个库 fake gateway 测试。原搜索 gateway 测试改为覆盖 controller→effect→result 整条链；原库查询测试验证真实第二页偏移量 60，而非仅检查初次请求。覆盖乱序成功/失败、重复响应、workspace/库隔离、排序失效、空结果、历史、过滤去重、自动分页失败后停止及手动重试。

本批最终检查：fmt 与 diff 检查通过；Clippy 通过（`/tmp/tiny-refactor-library-clippy.log`）；完整测试通过 1478 个引擎单元测试、2 个边界集成测试、712 个应用测试、6 个日志测试、3 个文档测试（`/tmp/tiny-refactor-library-tests-repeat.log`）。此前首次全量及一次独立运行出现既有引擎 `shutting_down_cancels_a_pending_body_read_without_waiting_for_network_timeout` 断言失败，保留输出 `/tmp/tiny-refactor-library-tests.log`、`/tmp/tiny-refactor-library-engine-retry.log`；未改动引擎，随后完整重跑通过，暂未确定间歇失败根因。

此批完成 Search/Library 子域迁移，尚未完成整个 Home。收藏、用户数据操作、详情、图片与 warm dashboard 的 controller/selector，以及 Playback、Shell 桥接和兼容层清理仍需继续。真实窗口/媒体 smoke 及最终六项验收仍未完成。

## 2026-09-29：Favorites 分页 controller 与共享用户数据模型

- FavoritesController 独立拥有三类收藏的分页状态、请求作用域、Enter/Refresh/LoadMore 和 remove/restore 操作；GPUI 滚动、carousel、grid 缓存与取消句柄迁入 FavoritesPresentation。HomeContent 不再通过可变分页字段执行加载或响应提交；渲染读取借用的 section VM。
- UserDataState 收拢唯一的 overrides、全局/条目/整剧 revision，并接管条目与继续观看响应合并、有效用户数据 selector。原六个覆盖数据测试随纯模型迁移。收藏响应依次校验 token/identity、按类型与 id 过滤、按 revision 与 pending mutation 合并用户数据、应用取消收藏覆盖、按原始数量推进分页；图片与通知只使用接受后的结果。保留重复响应条目的用户数据合并顺序。
- 新增 4 个收藏 controller 测试、1 个 fake gateway 完整链测试、1 个共享用户数据约束测试，覆盖分类隔离、dirty/跨 workspace 响应、乐观移除和恢复、重复条目、整剧 revision、pending mutation、刷新失败保留页面和手动分页重试。收藏元数据 fields、过滤器、排序、30 项页长和通知阶段保持。
- 全量检查再次发现既有引擎取消测试间歇失败。补充断言诊断确认实际是 `error sending request`，而非取消结果。mock 服务端此前 accept 后不读请求就发送响应；现先读取完整请求头再响应，并保留实际结果诊断。改动仅在该测试中，引擎生产代码和取消断言条件未变。

最终门禁：fmt、diff 检查和 Clippy 通过（`/tmp/tiny-refactor-favorites-clippy.log`）；全量测试通过 1478 个引擎单元测试、2 个边界集成测试、718 个应用测试、6 个日志测试、3 个文档测试（`/tmp/tiny-refactor-favorites-tests-final.log`）。失败及诊断输出保留在 `/tmp/tiny-refactor-favorites-tests.log`、`/tmp/tiny-refactor-cancel-diagnostic.log`。用户暂存的 Cargo 文件保持不变。

尚未完成收藏 mutation/rollback 请求的 controller 所有权迁移；已播放、继续观看操作、详情和其他 Home/Playback 任务也仍需统一。UserDataState 当前仍由这些待迁移操作写入，不能视作完整 Home controller 已完成。Shell、所有图像生命周期、UI API、旧 facade/代次清理及真实窗口/媒体 smoke 仍在最终验收范围内。

## 2026-09-29：收藏切换、回滚与保存约束

- FavoriteActions 统一拥有当前 workspace 的收藏 mutation、请求 slot、原始 override、移除条目的分类/位置和发起路由的通知上下文。HomeContent 原 favorite_requests/favorite_rollbacks 字段已删除；pending 由同一 operation 表推导，渲染、动作互斥、数据合并与快照暂停不再读第二份请求状态。
- ToggleFavorite → FavoriteCommand → HomeGateway.set_favorite → FavoriteUpdate 成为实际生产路径。新 `FavoriteMutation(item_id)` token 在结果提交前校验 identity 并消费请求；重复结果或同一条目上一轮的晚到结果不能更新 revision、回滚新操作、保存快照或发错误通知。GPUI 只持有可取消的 continuation，实体释放时停止结果投递。
- 原有互斥、乐观更新、收藏页面移除、失败恢复位置与 total、原始通知 scope/key、结束后的收藏刷新、写入期间暂停快照与完成后恢复保存顺序保持。NotificationScope/ActionNotification 成为纯模型，UI 队列仍保留原 SharedString 和自动隐藏行为。
- 新增 3 个纯 action controller 测试及 1 个 fake gateway 失败/重试链测试；既有收藏 rollback、条目菜单、播放状态更新、保存与 GPUI 布局测试全部保留。新增测试覆盖非法/忙碌输入、位置精度保留、跨 workspace、同条目重复结果、原 override/分类位置/total 恢复。

最终检查：fmt、diff、Clippy 全部通过（`/tmp/tiny-refactor-favorite-actions-clippy.log`）；完整工作区测试通过 1478 个引擎单元测试、2 个边界集成测试、722 个应用测试、6 个日志测试、3 个文档测试（`/tmp/tiny-refactor-favorite-actions-tests.log`）。引擎取消测试的 mock 握手修复也包含在此次通过的全量检查中。

下一步仍需迁移已播放与继续观看动作、Detail 的完整 controller/请求/导航选择、图片与 warm dashboard，随后收敛 Playback、Shell 桥接、UI API 和旧 facade。UserDataState 的剩余写入路径仍在上述范围中。用户暂存的 Cargo 文件未变；真实窗口与媒体 smoke 未执行，目标保持进行中，最终六项验收尚未完成。

## 2026-09-29：已播放与继续观看 mutation controller

- ResumeActions 拥有每个条目的独立请求 slot，取代 HomeContent 的 resume_item_requests。保留不同条目并行、同条目合并、已播放操作互斥及同条目收藏互斥。MarkPlayed/HideFromResume 通过 HomeGateway 保留原 Emby 入口；成功结果在纯 reducer 中使用最新有效用户数据、移除卡片并更新 total，失败不移除。GPUI 保存每项可取消 handle，接受结果后回收句柄，释放时统一取消。
- PlayedController 取代 HomeContent 的 played_request。纯 PlayedRequest/Command/Response 与 `PlayedMutation(item_id)` token 覆盖写入及随后整剧/分集读取。effect 保留 set_played → parent 或 episodes → selected item 的调用顺序；主写入失败不读后续接口，分集列表失败仍读取当前分集详情。
- 已验证结果先由 accept 消费 token、释放 pending；成功时页面先暂停旧快照保存，再让 PlayedCompletion reducer 更新数据，随后标记收藏 dirty、恢复 snapshot 调度。重复、上一轮和跨 workspace 的结果不会发通知、暂停保存或修改 revision。原通知上下文改用纯 ActionNotification。
- 整个已播放 reducer 从页面移出：保留 current/history/收藏/搜索/库/latest/resume 的 fallback 优先级、整剧 revision、未刷新分集只更新 revision、不伪造播放进度、最后一次 Items 响应优先、缺失 UserData 移除 override、继续观看 total、部分刷新失败的通知顺序。PlayedContent 只借用业务模型与条目引用，不持有 GPUI 资源或复制整份媒体数据。
- 删除不再使用的 bump_user_data_revision facade；共用的媒体数据 overlay helper 迁入纯模型。原“切季后旧整剧刷新返回”GPUI 测试升级为通过真实 controller/结果入口。新增 2 个继续观看 controller+fake gateway 测试、4 个已播放 controller+fake gateway 测试；其中明确断言并行乱序、互斥/失败重试、重复/跨 workspace、主写入失败短路、部分读取失败、最后响应优先、导航校验和进度精度。

本批四项门禁全部通过：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、728 个应用测试、6 个日志测试、3 个文档测试全部通过；`/tmp/tiny-refactor-home-mutations-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-home-mutations-clippy.log` |
| `git diff --check` | 通过 |

本批没有新增引擎或依赖改动，用户已暂存的 Cargo 文件保持。详情读取和播放地址解析仍使用旧 detail_generation，PlayedRequest 也暂时保留该导航约束；这不是最终统一 token 验收完成。下一步将 Detail 模型/请求/选择/历史与 GPUI 资源拆分，随后继续 Home 图像/warm dashboard、Playback、Shell/侧栏、UI API 与兼容层清理。真实窗口/媒体 smoke 仍未执行，最终六项完成定义未达成，目标保持进行中。


## 2026-09-29：Detail 数据、请求和选择 controller

- DetailController 拥有每个详情条目的 SeriesDetailModel 和六类独立请求 slot：媒体详情、相似作品、季、下一集、分集、继续观看的播放版本。原 DetailRequestRevisions 及六个按 generation 提交的加载入口已删除，所有读取通过 HomeGateway 和 DetailRequest/DetailResponse；保留原 Emby 参数与调用顺序，播放版本仍查询原 playable item 且不带 MediaSourceId 过滤。
- RequestToken 校验 workspace、owner、resource 和目标；分集额外检查当前 season/request season，再处理整剧 user-data revision。接受结果后纯 reducer 才合并用户数据、修改加载状态/选择、返回图片和通知结果。重复、跨账号、跨详情、切季后回到同季的旧响应均不能提交。NextUp 仍只应用已有 override，不吸收新用户数据；相似作品的过滤、total 和用户数据吸收顺序保持。
- Season/Episode/MediaSource/Subtitle 意图进入纯 controller；菜单、焦点、滚动、carousel、轨道偏好查询和 GPUI Task 继续属于展示适配器。删除 SeriesDetailState 的 Deref/DerefMut 和选择转发方法，视图显式借用 controller.view_model()，已播放与播放回写显式借用纯模型。没有新增逐帧媒体列表深拷贝；图片准备使用接受响应时的快照。
- 进入历史时清除请求 slot 并取消所有详情 continuation；切 root 时也先取消任务，再保留原两帧延迟释放数据/展示资源的流程。返回历史保留已完成数据、恢复未完成加载。六类详情读取不再使用 detail_generation，但当前/history 的聚合所有权、播放地址解析及 PlayedRequest 的导航约束仍待迁移。
- 新增 4 个纯 controller/fake gateway 测试和 1 个 GPUI 取消/副作用测试；原导航、播放版本、字幕、菜单、logo/分集布局等测试改为真实请求令牌。覆盖同目标重建、跨 workspace、重复提交、季往返、失败重试、错误响应类型、整剧修订冲突、端点参数、NextUp 失败后选择季，以及过期成功/失败不修改图片/用户数据/通知。GPUI 测试确认被取消的实际 continuation 不再投递。
- 全量检查中发现并修复了一个兼容细节：整剧操作已完成分集刷新时，旧分集结果只丢弃；只有仍处于 Loading 的旧请求才补发读取，避免新增网络请求。测试现在在 mutation 前持有旧 token，验证真实乱序而非调用已删除的 generation 入口。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、733 个应用测试、6 个日志测试、3 个文档测试全部通过；`/tmp/tiny-refactor-detail-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-detail-clippy.log` |
| `git diff --check` | 通过 |

详情聚合导航/历史及播放解析仍未完成，Home 图像/warm dashboard、完整 Playback、Shell/侧栏、其他 timer、UI API 与剩余兼容层继续按原规范推进。真实窗口和媒体 smoke 尚未执行，最终六项完成定义未达成，目标保持进行中。用户已暂存 Cargo.toml/Cargo.lock 未修改，本批也未修改引擎代码。


## 2026-09-29：详情激活令牌与 PlaybackGateway 起播解析

- 删除 HomeContent 的 detail_generation 及根路由、库、打开/返回详情中的裸计数更新。DetailController 拥有 DetailActivation slot；进入历史/卸载时失效，返回时重新 issue。PlayedRequest/PlayedContent 使用该 token 校验发起详情，避免“打开相同条目”或“离开后返回同一实例”接收旧整剧分集替换；全 workspace 的用户数据覆盖更新仍保持原语义。
- 新增 PlaybackGateway 与 EmbyPlaybackGateway。详情起播的 PlaybackInfo 查询、直链解析、HTTP headers、内容长度、分组条目 ID、返回媒体源 ID 和 play session 保留原实现参数与回退规则。字幕 URL 准备通过 port 提供，不由纯控制器访问 Emby 或服务器配置。该 port 目前承接详情起播；队列切换和播放报告仍需迁入同一边界。
- Prepared SelectedPlayback、播放队列选择和 DetailPlaybackCommand 是不含 GPUI 的类型，标题使用 Arc<str>。现有季上下文、分组版本、当前项优先、语言/保存轨道、字幕草稿和 tick 精度保留。选择准备发生在用户动作时；后台任务直接接管 command，再将 command 与结果一同返回，没有为投递额外深拷贝队列。
- DetailController 接管起播忙碌/失败状态和结果提交；DetailPlayback token 校验账号、请求 owner、当前条目和源。更换季/单集/播放版本、媒体源刷新改变选择或离开详情会失效请求并取消 continuation。错误保留字幕草稿；成功只消费一次草稿并返回 Open 结果，GPUI runner 才构造原 PlaybackRequest/EmbyPlaybackContext 和兼容事件。重复、旧请求或跨账号成功/错误不发事件、不覆盖新状态。
- 新增 3 个纯 controller/effect 测试、1 个 PlaybackGateway 实际 HTTP 适配测试、1 个 GPUI continuation 取消测试。验证分组 item/list/source 的区别、队列与位置、错误后重试、草稿转移、重复/跨账号/版本往返、详情离开返回的激活令牌、原 POST/query/device profile 和完整传输元数据。原字幕草稿及继续观看版本 GPUI 测试已改为先发起真实 command 再交付响应，原整剧导航测试也使用真实激活 token。

本批最终验证：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、738 个应用测试、6 个日志测试、3 个文档测试全部通过；`/tmp/tiny-refactor-detail-launch-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-detail-launch-clippy.log` |
| `git diff --check` | 通过 |

代码搜索确认 src 中已无 detail_generation/DetailRequestRevisions，详情生产代码也不再直接调用 playback_info/resolve_direct_stream_url 或 detach 起播任务。当前/history 的聚合所有权仍与 GPUI entry 组合，尚需拆成 Home/Detail 导航 controller 与独立展示资源；Home 图像/warm dashboard、Playback reporting/queue/backend/controls、Shell/侧栏、其他 timer、UI API 与真实窗口/媒体 smoke 仍在完整目标内。四项自动门禁不构成最终六项验收完成。用户暂存的 Cargo 文件保持，本批没有引擎实现改动；目标保持进行中。


## 2026-09-29：详情历史栈与展示资源所有权拆分

- HomeNavigation 现在同时拥有路由栈、当前 DetailController 和历史控制器；HomeContent 的 series_detail/detail_history 字段及 SeriesDetailState 生产类型已删除。打开详情、返回、切 root、打开库/收藏分类由纯导航转换处理，NavigationChange 返回隐藏条目 ID 与已经失效的移除模型。标题选择和继续观看版本恢复也由导航 owner 处理。
- 每个详情有独立 DetailId，仅用于展示资源索引，异步有效性继续使用 RequestToken。相同媒体的不同详情实例不会共享滚动、菜单或请求；隐藏后保留原控制器和展示资源，返回复用原 DetailId 并重新激活请求。新建详情不复制其他历史条目的展示句柄。
- HomeContent 仅保存按 DetailId 索引的 DetailResources（presentation、数据读取和起播取消句柄）。导航转换后适配器取消隐藏/移除条目的任务并清除旧 overlay；普通返回立即释放离开条目，切 root 仍将移除模型和对应展示资源保留到第二个 frame callback 再释放，保持原窗口刷新时序。
- DetailView 是只含 `&SeriesDetailModel` 与 `&SeriesDetailPresentation` 的可复制借用视图，渲染不能取得可写 controller。DetailBinding 仅在 runner/输入适配器中临时组合可变借用，不拥有模型、不进入页面或历史栈。删除原隐式字段转发后，各 renderer、carousel、菜单、播放回写及用户数据 reducer 已接入分离后的所有者；没有逐帧详情数据克隆。
- 已播放 reducer 通过 detail_models_mut 仅借用当前/历史媒体模型，不能修改历史结构、DetailId 或 activation slot。导航范围内的业务查找直接读取纯模型；测试场景的临时 DetailFixture 仅在 cfg(test) 下编译，并在安装时拆为模型和展示资源，不形成生产兼容层。
- 新增 4 个纯导航测试和 1 个 GPUI 回归测试：相同媒体实例隔离、隐藏请求失效/返回重新激活、root 清空且移除 payload 仍可延迟释放、库/收藏来源与标题、继续观看版本恢复，以及通过返回按钮恢复原单集和 600px 滚动位置、移除退出条目的资源、切 root 后执行两帧释放。原全部交互、回写、缓存、图片和布局测试保留。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、743 个应用测试、6 个日志测试、3 个文档测试全部通过；`/tmp/tiny-refactor-detail-owner-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-detail-owner-clippy.log` |
| `git diff --check` | 通过 |

代码搜索确认 src/home 与 src/home.rs 中不再保留 series_detail/detail_history 数据字段、SeriesDetailState、detail_generation 或 Deref facade。本批没有修改引擎或用户暂存的 Cargo 文件。Home 其余状态聚合、图像/resize/warm dashboard、Playback reporting/queue/backend/controls、Shell/侧栏、其他 timer 和整体 UI API 仍需完成；GPUI 自动测试不替代真实窗口与媒体 smoke，最终六项验收仍未达成，目标继续进行。


## 2026-09-29：Home 业务聚合、播放回写与媒体库资源拆分

- 新增无 GPUI/IO 的 HomeController，唯一持有工作区 identity、导航、Feed/Search/Library/Favorites controller、UserDataState、播放版本、三类 mutation controller 和播放返回刷新 slot。HomeContent 删除对应平铺字段，仅保存控制器、展示资源和投递句柄；没有新增 Deref 或平行状态副本。现有 renderer/runner/测试改用明确的 owner 路径，后续继续收敛可写 API 与统一 VM/Intent。
- 跨列表 user/resume 查找、播放用户数据来源优先级、覆盖 selector 与读响应吸收迁入控制器；删除页面上的同名转发/规则实现。保留当前 route 优先及 latest/favorites/search/library 回退顺序，播放数据仍按 override→当前详情→resume→latest→library→favorites→search 查找。selector 借用底层数据，未增加逐帧整表深拷贝。
- 播放返回 reducer 保留 actual/list ID 同步更新、单次 revision 增长、tick 精度、完成后移除 resume 项、用户数据其他字段及各源轨道选择。DetailUpdate 与 DetailId 返回展示适配器；适配器只更新展示、取消起播投递、调度保存和等待 stop 报告。HomePlaybackRefresh token 的 latest/identity/单次提交判定及详情刷新标记由纯控制器执行；250ms×160 轮询和成功刷新条件保持。整剧已播放结果的当前/历史/list 数据聚合也从页面移入该 owner，保存暂停仍在接受响应后、修改模型前执行。
- 快照导出、晚到缓存的版本/轨道补缺、全局轨道偏好合并与导出属于 HomeController。时间戳显式传入纯快照构造；文件路径、JSON version/字段、权限、替换、450ms debounce 和最终 flush 协议保持。缓存补缺仍不覆盖 live source/name 或明确的 track choice，快照包含有效用户数据而不修改已加载原始列表。
- 删除 LibraryState 组合所有者。HomeController.libraries 仅保存 LibraryController，HomeContent.library_resources 按库 ID 保存 LibraryResources（Task handle、LibraryPresentation）。LibraryView 只借用 LibraryVm 与 presentation，无法取得控制器或任务。打开库、分页/sort intent 分发、响应提交及 revision overlay 吸收由 HomeController 处理，旧/跨账号/重复响应在图片及通知处理前拒绝；原 60 项分页、排序参数、通知 key、滚动复用和取消行为保持。
- 新增 7 项纯聚合测试，涵盖 route 查找与播放数据优先级、双 ID 回写和旧读隔离、失败/空播放源、完成移除、停止刷新 token、库响应与 overlay、缓存/轨道补缺和 JSON/非破坏导出；原 2 项播放计算测试迁入纯模块。删除 1 项仅测试旧快照 helper 的测试，用实际 controller 快照测试替代；新增 1 项 GPUI 回归验证库重开保留 600px 滚动、复用资源和选择当前排序只关闭菜单。应用测试从 743 增至 750。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、750 个应用测试、6 个日志测试、3 个文档测试全部通过；`/tmp/tiny-refactor-home-owner-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-home-owner-clippy.log` |
| `git diff --check` | 通过 |

本批没有修改引擎或用户暂存的 Cargo.toml/Cargo.lock。Home 图片、resize/warm dashboard 和剩余 view/controller API、Playback reporting/queue/backend/controls、Shell/侧栏、其他 timer、完整依赖边界检查与真实窗口/媒体 smoke 仍在原目标范围内。最终六项完成定义尚未全部达成，目标保持进行中。


## 2026-09-29：首页及播放剧集封面的 ImageRepository 与请求所有权

- ImageRepository 现在按请求类型提供关联结果，服务器图标继续由 FileImageRepository 处理，媒体图片由绑定 immutable Emby client/server 的 EmbyImageRepository 处理。没有默认“不支持”方法或 UI/Context 跨 port。公共图标继续不携带 Emby 凭据；媒体图片保留原 item image GET、maxWidth/tag/quality 与认证 header，repository 的 Debug 不输出服务器或凭据。
- 删除 images/loader.rs 的 ImageLoader，新增无 GPUI/IO 的 ImageController。它拥有 workspace identity、排队/在途/失败与已完成路径索引；所有 key 经 ItemImage scope 的 RequestSlot 发起，完成时校验 identity 和当前 slot 并单次 commit。旧尝试、旧 page owner、跨账号、跨 key、重复成功/错误都不更新路径或失败状态、不释放新请求的并发名额。重试时间由调用者显式传入，单元测试无需真实等待。
- 原 UI 回调中的磁盘缓存 exists 检查移入后台 repository；下载、扩展名判定、缓存写入和 512 MiB 淘汰也只在该 adapter 执行。保留 local server/item/type/tag/width/quality 路径、legacy img 查找、目录 0700/文件 0600、临时文件重命名和 best-effort prune 行为；路径索引继续返回同一 Arc<Path>，借用查询不逐帧构造或深拷贝缓存 key。
- HomeContent 的 images 属于展示域，image_repository 与 image_effects 独立保存。图片结果被控制器接受后才移除对应 handle 和补充排队任务；仅 Ready 更新通知视图，失败不产生业务通知。取消后已开始的阻塞网络/磁盘 IO 可能完成，但不能回写已释放或不匹配页面；释放会取消投递 continuation 及尚可取消的后台 await。路由间保留 workspace 图片缓存，与原 warm-dashboard/返回行为一致；新 tag/尺寸是独立 key，旧图片不会覆盖新查询。
- PlaybackEpisodeListState 接入同一纯调度器与 ImageRepository，删除直接 Emby/缓存调用和 detached 图片投递。保留 4 个并发请求、从当前集附近优先排队、关闭抽屉后不启动剩余队列、已开始结果可填充本页面缓存、重开复用图片和页面替换/释放取消。Home 保留 100 个并发请求，两处保持 30 秒重试间隔和最多 3 次尝试。媒体帧/字幕的展示与延迟释放流程本批未修改。
- 原 4 项 loader 测试被 6 项纯 controller 测试取代，覆盖去重、并发、精确重试边界、失败计数、旧响应隔离和 Arc 路径/参数区分；新增 3 项真实 HTTP/文件 adapter 测试验证参数/认证、离线复用、legacy 格式、权限及下载失败不写缓存；新增 4 项 GPUI 测试验证 fake repository 驱动队列与 Ready 通知、旧响应保留当前 handle、释放取消 timer/await、抽屉关闭/重开行为和请求参数。原布局测试使用 cfg(test) 显式种入展示路径，生产代码没有跳过 token 的完成入口。应用测试从 750 增至 759。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、759 个应用测试、6 个日志测试、3 个文档测试全部通过；`/tmp/tiny-refactor-image-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-image-clippy.log` |
| `git diff --check` | 通过 |

源代码审计确认 Home 与剧集封面 runner 中不再调用 EmbyClient.item_image 或缓存文件 IO，也无 ImageLoader/ImageLoadJob 兼容类型。用户暂存的 Cargo 文件及本批引擎实现未改动。Home resize/warm dashboard、其余 VM/Intent API、Playback reporting/queue/backend/controls 与媒体展示图像边界、Shell/侧栏、其他 timer、完整依赖审计及真实窗口/媒体 smoke 仍在完整目标内；自动 GPUI 测试不替代真实 smoke，最终六项完成定义尚未达成，目标继续进行。


## 2026-09-29：Home resize 与 dashboard 缓存策略所有权

- 新增纯 HomeLayoutController，拥有窗口尺寸观测、resize burst、列收缩策略、dashboard 预热和路由返回复用/内容脏标记。HomeContent 删除 last_window_size、resize_in_progress、resize_generation、last_resize_activity、resize_settle_task_active、defer_workspace_grid_contraction、defer_home_dashboard_warmup、workspace_grid_columns 和 reuse_home_dashboard_until_next_render 等平铺字段。布局 VM 提供 grid columns、carousel/grid overscan、auto pagination 和 dashboard mount 判定，各 renderer 只读取该 VM。
- HomeResize scope 管理一次完整拖动：连续 bounds 变化只更新最后活动时间，不重复分配任务或 token；静止 120ms 后单次提交，释放延迟收缩。HomeDashboardWarmup scope 管理收尾后的两帧 callback；新的 resize 立即失效旧 warmup，跨账号/旧 settle/旧 warmup/重复提交均不改变状态。原缩放中禁止 auto pagination/overscan、至少两列的突然收缩保持旧布局、扩展立即布局，以及 Home 页面始终可挂载的规则不变。
- HomeDashboardResources 独立保存缓存 entity 和 EffectHandle，home/layout.rs 只执行 timer/GPUI callback/notify。保持先提交最终 workspace 网格，再嵌套两个 next-frame callback 预热隐藏首页；弱 entity 和 token 同时防止释放后提交。计时使用 GPUI executor 的单调时钟，生产仍为 Instant，测试可精确推进 119ms/120ms，无需 sleep 或真实时间轮询。
- 首页缓存复用策略由纯 controller 负责。纯路由返回仍跳过缓存失效，首帧之后的 Home 更新正常重绘；增加内容脏标记，修补后台数据/图片或用户数据变更与“直接复用”冲突的边界。接受的 feed 读结果、图片 Ready、列表/详情覆盖、收藏乐观变更和回滚、继续观看/已播放/播放返回会标记内容；过期结果不触及标记。隐藏期间保留缓存，返回时才失效，以保证复用不展示过期数据。
- 原 3 个纯 helper 测试迁入布局模型；新增 4 个控制器测试验证同 burst 单 token、最近 bounds 的防抖截止、重复/跨账号/旧 token、延迟收缩与实时扩展、Home/auth 路由条件及数据优先于复用。新增 3 个 GPUI 测试直接观察实际 HomeDashboard 渲染次数及两帧回调，覆盖最终网格先于预热、新 resize 取消旧 frame 的权限、route-only 返回不重建，以及隐藏数据和 fake repository 图片结果到达后返回必须重绘。原数百次窗口宽度变化保持固定边距的回归继续通过；render_count 仅在 cfg(test) 下存在。应用测试从 759 增至 766。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、766 个应用测试、6 个日志测试、3 个文档测试全部通过；`/tmp/tiny-refactor-layout-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-layout-clippy.log` |
| `git diff --check` | 通过 |

代码审计确认 Home 没有 resize_generation 或旧实体布局标志/Task，并且布局模型不依赖 GPUI、Window、Context 或文件 IO。本批未改动引擎实现或用户暂存的 Cargo 文件。剩余完整目标包括 Home 与通用业务组件 VM/Intent API 收敛、Playback reporting/queue/backend/controls/presentation、Shell/侧栏及其他 timer、依赖边界审计和真实窗口/媒体 smoke。自动 GPUI 测试仍不等同真实 smoke；最终六项完成定义未全部达成，目标继续进行。

## 2026-09-29：Playback reporting 控制器与串行 effect

- `PlaybackPage` 的启动、进度、停止策略迁入无 GPUI 的 `ReportingController`。控制器拥有 Prepared→Started→Closed 状态、精确 ticks/音量/轨道快照、物理条目与分组队列 ID 映射、周期 token 和幂等关闭结果；页面只组装 telemetry/context 并把 intent 交给控制器。
- 新增 `PlaybackGateway::report` 与 `EmbyPlaybackGateway`，上报请求由单独 effect worker 串行执行。mailbox 只保留一个 start、最新一份 progress 和 terminal stop；stop 清除排队 progress、等待当前 IO 完成后执行，并在页面释放后继续存活。RequestSlot/WorkspaceIdentity 校验阻止跨账号命令，receipt 在发送失败、worker panic、HTTP 失败或命令丢弃时收敛为 Failed。
- 周期进度使用可取消的 `EffectHandle<Task>`，关闭/释放会失效 timer；页面释放仍发送一次停止报告。reporting worker 使用静态 operation/phase trace，三个 Emby 上报方法的 span 不再附带 endpoint/条目 ID；新 worker 不输出原始错误内容（公共 HTTP 客户端既有诊断日志策略另行审计）；HTTP adapter 仍保留原三条 POST 路径、请求体和认证头。
- 新增纯 controller 测试覆盖空会话失败、分组队列与轨道快照、重复/强制进度、跨账号/过期 timer、结束位置、停止回执 CAS 和关闭后快照；effect 测试覆盖 10000 次进度合并、慢起播期间 stop 优先、跨账号丢弃、worker panic 清理和 start-before-stop 顺序；GPUI 测试覆盖 10 秒计时、暂停/静音、关闭及实体释放；HTTP 测试验证三种 endpoint/body/认证和失败回执。应用测试从 766 增至 780。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、780 个应用测试、6 个日志测试、3 个文档测试全部通过；`/tmp/tiny-refactor-reporting-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-reporting-clippy.log` |
| `git diff --check` | 通过 |

本批没有修改引擎或用户暂存的 Cargo.toml/Cargo.lock。Playback queue/backend/controls/presentation、Home 与通用业务组件 VM/Intent API、Shell/侧栏、其他 timer、完整依赖审计及真实窗口/媒体 smoke 仍在完整目标内；自动 GPUI/mock HTTP 测试不替代真实 smoke，最终六项完成定义尚未全部达成，目标继续进行。

## 2026-09-29：Playback 队列控制器、gateway 解析与取消

- 新增 `player/queue` 的纯 model/controller 与 source-resolution effect。`QueueController` 成为队列、当前索引、pending action/recovery 和错误的唯一 owner；页面移除独立 `PlaybackQueueSwitchState`。Previous/Next/Select 的目标边界、重复加载合并、暂停失败、手动失败恢复与自动失败回写由控制器处理，渲染读取借用的 QueueViewModel/queue selector。
- `PlaybackQueue` scope 的 RequestSlot 取代裸 generation。成功、失败、暂停失败都校验 token、workspace 和 owner，接受一次后消耗；取消/返回/后端失败/释放使旧请求失效。页面保存 `EffectHandle<Task>`，替代 detached continuation；旧结果不取消新 handle、不更新错误、不发送 Replace/Update。gateway 为页面绑定的不可变 Emby snapshot，阻塞 IO 可在取消后结束但不回写。
- 解析 effect 通过 `PlaybackGateway::resolve_source` 和 `subtitle_tracks`，页面不再直接调用 Emby PlaybackInfo、解析直链或组装认证头。保留默认媒体源优先级、分组物理 ID、请求源 track preference key、解析后的字幕地址、语言/保存轨道覆盖、断点精度和空 PlaySessionId 过滤。controller 的 QueueReplacement 合并上一集位置：正常切走保留精确 ticks，已结束置 0；selected_item_id 仍使用目标列表条目，外部 PlaybackEvent 契约不变。
- 原 3 个 switch state 测试迁入并扩为 7 个 controller 测试，覆盖暂停/末尾恢复、边界/重复/跨账号/跨 owner/取消后晚到、各 action 错误、停止快照合并；新增 4 个 fake gateway 测试覆盖物理 ID/transport/轨道/无源/失败；新增 4 个 GPUI 测试覆盖手动恢复缓存暂停、自动失败回写、旧结果保留当前 handle/只一次 Replace，以及返回与释放取消。已有真实 HTTP 非相邻切集与剧集 UI 测试继续通过。应用测试从 780 增至 792。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、792 个应用测试、6 个日志测试、3 个文档测试全部通过；`/tmp/tiny-refactor-queue-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-queue-clippy.log` |
| `git diff --check` | 通过 |

审计确认切集代码没有 generation、detach 或直接 Emby 请求；本批未改动引擎与用户暂存的 Cargo 文件。剩余完整目标包括 Playback backend reducer/adapter、SourceState/controls/presentation、Home 与通用组件 VM/Intent、Shell/侧栏、其他 timer、边界审计和真实窗口/媒体 smoke；不能以 mock/GPUI 自动测试代替人工验收。目标保持进行中，未宣称全部完成。

## 2026-09-29：Playback 会话与后端事件 reducer、媒体状态和暂停轮询

- 新增无 GPUI 的 `PlaybackSessionController`，聚合 timeline、SourceState、QueueController 和 ReportingController。页面不再平铺持有 timeline/queue/reporting、source URL/protocol/content length、媒体信息、轨道选择及起播字幕偏好标记。轨道菜单开关独立为页面展示状态，其他 controls 的直接写入继续迁移到 intent，未把该阶段包装成最终 UI API 完成。
- 原 `BackendEventKind` 的时间线分支、终止顺序与自动切集判断迁入 `reduce_backend`。Restart 先更新 loaded/pause/seek 再生成 Started/Progress；Ended 先清除拖拽/seek 并确定末尾位置，顺序产生 final Progress、Stopped 和页面 Update/自动切集动作，再清理暂停/缓存状态；失败先截取旧拖拽/位置/用户暂停/CanSeek 快照生成 Stop，再取消队列并重置 timeline/媒体信息。已关闭的 restart 与重复 ended 不再产生业务动作。
- SourceState 处理视频、文件、音频元数据、尺寸更新和后端轨道结果。保留已加载外部字幕，后端自动更换字幕只清除起播偏好提交标记，保持原选择时才允许后续起播保存。页面只执行诊断输出、菜单/缓存 popover、字幕图像与视频帧处理、报告投递和原 PlaybackEvent；本批未改变 native backend/presenter 的 ShutdownOrder 或图像延迟释放。
- 暂停期间缓存轮询从 page boolean + detached timer 迁为 PlaybackBackendPoll RequestSlot 与页面 EffectHandle。保留 250ms 和 byte/demux/cache 活动判定；一次只有一个 timer，回调核对账号/owner/单次提交并重新检查 backend/error/播放状态。结束、失败、返回和释放失效 token/取消 handle，已关闭 reporter 不再启动轮询。
- 新增 11 个纯会话测试覆盖旧状态先于失败清理、末尾上报与自动切集顺序、缓存暂停/拖拽/soft seek、1000 次事件与重复终止、轨道与偏好、元数据/尺寸、旧/跨账号/取消/关闭后的轮询；新增 1 个 GPUI 返回取消轮询测试。原后端事件、字幕缓存、轨道偏好、切集、上报和鼠标测试继续通过，应用测试从 792 增至 804。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、804 个应用测试、6 个日志测试、3 个文档测试全部通过；`/tmp/tiny-refactor-backend-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-backend-clippy.log` |
| `git diff --check` | 通过 |

本批未改动引擎实现或用户暂存 Cargo 文件。剩余完整目标仍包括 native PlaybackBackendAdapter 所有权/可控 fake、controls/presentation intent/VM、后端事件 session 身份边界审计、App Shell/侧栏、Home/通用组件 API、其余 timer、整体依赖检查和真实窗口/媒体 smoke；现有自动测试不能证明这些未完成条目。目标继续进行。


## 2026-09-29：PlaybackBackendAdapter 原生资源边界与可控测试

- 新增 `player/backend`，页面现在只持有 `PlaybackBackendAdapter`，无法取得或替换原生 decoder/presenter。`PlaybackDriver` 和 `FramePresenter` 提供命令、已验证事件、呈现帧和诊断快照；生产实现保持原 FFmpeg `BackendControl` 委托，fake 不初始化 FFmpeg、Vulkan 或音频设备。错误分别保留 backend 的类型和 renderer 的 anyhow context。
- 原构造过程迁入 adapter：创建 backend、创建 presenter、恢复音量/静音、Load。音量失败不启动 Load；presenter 失败保留 backend；Load 失败保留两者直到页面释放。初始缓存规范化、runtime-only 默认目录、请求头、源地址、断点精度和默认/外置轨道值保持不变；运行时缓存设置同样经过 adapter。
- `ShutdownOrder` 连同原测试迁到 backend/lifetime；实际代码和历史测试的契约是 **先 presenter，后 backend**。规格 §5.3 的“presenter 后于 backend”与既有代码不符，本批按 §8 保留现有释放顺序，不修改原始规格文件。BgraImage 仍以所有权转移传出，不增加整帧复制；页面图像/字幕的延迟回收路径保留。
- session 身份审计确认 FFmpeg 在 drain_worker_events 和视频尺寸通知处校验 current_session_id，VideoPresenter 在 session/presentation generation 变化时废弃旧帧；已有引擎 stale session 回归继续覆盖真实过滤。adapter 仅转发已经验证的事件，不根据首个返回事件猜测 session，也不重建应用层代次。
- 新增 9 个 adapter 测试覆盖各初始化失败点、命令/缓存映射、事件顺序、帧分配转移、渲染失败及释放；新增 6 个 GPUI 测试覆盖命令失败状态保留、缓存暂停下恢复/seek/音量事件、渲染失败关闭与释放、250ms 暂停轮询，以及切集暂停/恢复失败。轨道拒绝测试改用可控 fake。应用测试从 804 增至 819。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、819 个应用测试、6 个日志测试、3 个文档测试全部通过；`/tmp/tiny-refactor-runtime-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-runtime-clippy.log` |
| `git diff --check` | 通过 |

本批没有修改引擎实现或用户暂存 Cargo 文件。剩余完整目标包括 controls/presentation intent/VM 与计时器、App Shell/侧栏、Home/通用组件 API、整体依赖检查及真实窗口/媒体 smoke；自动 fake/GPUI 测试不替代真实 smoke。目标仍进行中。


## 2026-09-29：播放展示计时器统一 token 与取消

- 新增纯 `PresentationTimers`，以四个独立 RequestScope 管理控制栏隐藏、音量提示、倍速提示和下载速度刷新。控制栏/提示的最新操作替换旧 deadline，下载速度在待刷新期间合并请求并保留最早的一秒截止时间；所有回调核对 workspace、scope、owner 和当前 token，单次 commit。
- 页面 `PresentationEffects` 只保留四个 EffectHandle<Task>。替换会释放旧 continuation，悬停返回按钮/重置全屏取消控制栏任务，返回详情永久关闭全部 slot 并取消全部任务，实体释放自动回收。回调先接受 token 再取消自己的 handle，旧回调不影响新任务；这些本地计时器无网络/IO，也没有错误通知 key。
- 删除 fullscreen、volume、rate 的裸 hide_generation 和下载速度 refresh_scheduled，以及相关 detached timer。保留控制栏 1000ms、音量/倍速 1200ms、下载速度 1000ms 的规则，保留 episode drawer、hover、progress drag 对隐藏的阻止，速度浮层不可见时不通知。提示到期只在可见状态确实变化时 notify。
- 新增 4 个纯模型测试覆盖千次替换/合并、跨 scope/owner/account、重复完成、取消和关闭；新增 4 个 GPUI 测试覆盖提示独立时限、连续动作、控制栏全部阻止条件与 hover 取消、旧回调保留新任务、返回和释放取消四类任务，以及速度使用最新值/隐藏后再显示。应用测试从 819 增至 827。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、827 个应用测试、6 个日志测试、3 个文档测试全部通过；`/tmp/tiny-refactor-presentation-timers-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-presentation-timers-clippy.log` |
| `git diff --check` | 通过 |

源码审计确认 `src/player` 不再有 generation 字段/比较，播放页剩余 detach 均为 GPUI subscription 的既有生命周期用法。本批未改动引擎或用户暂存 Cargo 文件。controls/presentation 的业务 owner 与 intent/VM API 仍需继续收敛，App Shell/侧栏、Home/通用组件、其余 feature timer、整体依赖审计及真实窗口/媒体 smoke 仍在完整目标内。最终完成定义尚未全部满足，目标继续进行。


## 2026-09-29：Playback controls intent、只读 selector 与会话状态封装

- 新增 `session/controls`，`PlaybackIntent` 覆盖暂停、音量增减/静音、倍速、绝对/相对 seek、拖拽预览及音轨/字幕选择。控制器通过注入的同步命令执行器接收 backend 结果，输出提示、保存轨道、强制上报、VolumeChanged、菜单关闭和字幕清理等动作；该执行器不能越过会话的可变借用存活，不新增异步回调或原生对象依赖。
- 会话私有 ControlState 持有音量/上次非静音值、倍速与错误，页面仅保留两个提示可见标志。初始化错误、backend failure、render failure 和 restart/end 的错误清理由同一会话 owner 处理。render failure 仍先形成用户暂停/原 CanSeek/拖拽位置的停止快照，再写入错误；原页面上报路径也使用 controller 的统一 telemetry。
- timeline 与 source 字段改为私有；生产页面只能借用 timeline()/source_view()/controls_view()，没有可写引用。queue 暂停及失败恢复、首次字幕偏好消费也迁入会话方法，所有业务写入留在会话内。仅 cfg(test) 的 fixture mutation 入口服务既有交互测试，发布构建不提供这些接口。
- 控制栏和轨道菜单读取 PlaybackControlsViewModel/TrackChoices，保留 borrowed track slices，不复制整个轨道列表。进度百分比、当前/时长标签、缓存 seek 预览、forward cache 和 ranges 迁入纯 progress model，并由 ProgressTimelineViewModel 统一派生；窗口坐标换算、hover 几何、GPUI 事件和延迟字幕回收留在页面。
- 保留细节：无后端时仍能保存音量而暂停/倍速不提交；音量只跨越静音边界才强制上报；relative seek 优先 drag→pending→position，保留 Fast 命令参数；seek 失败保留原 position/buffered/end，清除 drag/pending/buffering；切轨失败保留选择和当前字幕图像，不写偏好；关闭轨道不擅自清除既有 buffering；预览通知保留 0.02 秒阈值。轨道偏好 physical/grouped/requested/resolved key 合并和未知轨道拒绝策略已移入纯 selector。
- 新增 10 个不依赖 GPUI/网络的控制器测试，覆盖命令拒绝/缺失、暂停与缓存暂停、音量恢复和保存、倍速上界、缓存 seek/位置精度、失败回滚、relative 优先级、字幕副作用、预览阈值/borrowed VM 和偏好 key。原 3 个音量测试迁入 model；所有原 GPUI、上报、队列、鼠标和布局回归继续通过。应用测试从 827 增至 837。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、837 个应用测试、6 个日志测试、3 个文档测试全部通过；`/tmp/tiny-refactor-controls-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-controls-clippy.log` |
| `git diff --check` | 通过 |

本批未修改引擎或用户暂存 Cargo 文件。剩余完整目标仍包含 presentation aggregate/组件 callback API、Home 与通用组件的最终 selector/intent 收敛、App Shell/侧栏、其余 feature timer、完整依赖检查，以及真实窗口/媒体 smoke。自动测试不替代这些未验收条目，最终六项完成定义仍未全部证明，目标保持进行中。


## 2026-09-29：App Shell 路由、挂载身份与兼容事件入口

- 新增不依赖 GPUI 的泛型 ShellController，持有根路由和 opaque 页面/订阅句柄。AppRoute 只暴露 Servers/Home(workspace)/Playback(workspace)，MountedPage 是只读实体投影；TinyApp 不再直接写页面枚举。mount Home、mount/replace Playback、return Home 和 show Servers 是唯一状态入口，窗口级 overlay 继续独立于路由。
- Home 与 Playback 各有 AppHomeMount/AppPlaybackMount RequestSlot，绑定不可变 WorkspaceIdentity；订阅事件携带该挂载 token，经顶层 AppIntent 校验后才协调路由、设置/弹窗、音量/搜索历史保存和播放返回更新。即使同账号重新挂载，也拒绝旧 owner 的事件。保持当前 Home 在播放期间接收合法业务结果，替换播放和返回时复用同一个 Home entity/订阅。
- 两类 GPUI subscription 由 ShellController 的 EffectHandle 持有，移除原 detached subscription。替换、返回、切换服务器和壳释放分别注销退役订阅、失效 token，再释放页面；已进入 runner 的旧事件仍由 token 阻断。页面释放时原 Home persistence 与 Playback stop reporting/图像回收流程继续运行。
- 5 个纯模型测试覆盖实体复用、同 workspace remount/跨用户/跨 shell token、Servers 后孤立播放拒绝、精确订阅注销和释放；2 个 GPUI 测试通过真实 HomeEvent/PlaybackEvent 订阅验证旧 Home 不能改路由/overlay/history、旧 Playback 不能改音量或返回路由、当前事件仍生效，以及返回后保留 Home 的订阅。应用测试从 837 增至 844。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎单元测试、2 个边界集成测试、844 个应用测试、6 个日志测试、3 个文档测试全部通过；`/tmp/tiny-refactor-shell-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-shell-clippy.log` |
| `git diff --check` | 通过 |

本批保留用户暂存 Cargo 文件和引擎实现。侧栏 display model/reorder 与 props/callback API、Home/通用组件最终收敛、presentation aggregate、其余 timer、整体依赖审计与真实窗口/媒体 smoke 仍未全部完成；完整目标保持进行中。

## 2026-09-29：侧栏展示投影、重排控制器与独立 view

- ServerController 在目录变更时生成仅含 ID、标题和图标的 SidebarServer 投影；HomePage 不再复制完整 CachedServer 目录或当前服务器认证记录。标题保留原有空字符串回退、非空名称不裁剪的行为及持久化目录顺序。
- 纯 SidebarController 持有当前 ID、用户名、展示投影、切换状态与临时重排；所有写入经 SidebarIntent，产生 Changed/Select/Reorder 命令。selector 借用行数据并仅重排索引，render 不再克隆完整服务器目录；相同 props 不触发无效 notify。
- 侧栏 view 仅接受 SidebarProps 和 intent callback，不依赖 HomePage、CachedServer 或 EmbyClient。GPUI adapter 保留焦点恢复、Escape、滚动可见范围、拖拽 owner 校验、Home 双击刷新和 HomeEvent 契约。拖拽提交仍按原目录 target index 解析，预览不写目录；目录缩短、取消和重复完成保持原语义。
- 5 个纯 controller 测试覆盖切换/取消认证、借用预览、提交一次、无效索引、目录变化与 loading；1 个投影测试验证标题回退和目录顺序。原 GPUI 拖拽、滚动、取消、选择和固定行高测试全部通过。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎测试、2 个边界测试、850 个应用测试、6 个日志测试、3 个文档测试通过；`/tmp/tiny-refactor-sidebar-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-sidebar-clippy.log` |
| `git diff --check` | 通过 |

用户暂存 Cargo 文件保持不变。Home/Playback 最终组件 API、presentation aggregate、其余 timer、整体依赖审计及真实窗口/媒体 smoke 仍待完成。

## 2026-09-29：Playback 展示资源聚合与页面替换释放验证

- PlaybackPresentationState 统一持有焦点、帧/视口、字幕图像、全屏控制、窗口拖动、进度 hover、菜单、剧集列表、下载速度、音量/倍速指示器和已统一的展示 timer。PlaybackPage 保留 session、backend adapter、Emby 上下文和业务 effect；纯 timeline 从 model 直接引用，移除原混合 state 模块。
- 构造和释放入口统一，release_images 幂等取出帧/字幕图片并关闭展示 timer，随后继续走原 deferred 两帧图集回收。Back、失败和切集原有清理/回报顺序保留；PlaybackBackendAdapter 内部 ShutdownOrder 未更改。
- 新增 GPUI 回归测试通过无原生资源的假 backend/presenter 驱动实际 PlaybackPage，替换页面后确认旧实体释放，视频和字幕图片在第一帧仍留在图集、第二帧才移除。计时器、全屏鼠标交互、控制栏、字幕失败保留和剧集图片生命周期的已有测试全部通过。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎测试、2 个边界测试、851 个应用测试、6 个日志测试、3 个文档测试通过；`/tmp/tiny-refactor-presentation-state-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-presentation-state-clippy.log` |
| `git diff --check` | 通过 |

上述验证不替代真实窗口/原生媒体 smoke；整体目标尚未完成。

## 2026-09-29：播放选源/轨道规则与起播请求边界

- 原 page/request.rs 的源优先级、音轨/字幕流选择和有效 stream index 匹配移入 model/selection；Emby tick 常量及起播位置校验移入 model/time。Home launch、队列 effect 和 selector 的业务调用不再经由页面模块，纯模型不引入 GPUI 或 IO。
- 外部字幕识别、delivery URL 回退、路径解析与 api_key 补充进入 adapter/subtitles，由 EmbyPlaybackGateway 调用，原 URL/认证参数行为保持。PlaybackRequest/EmbyPlaybackContext 作为不可变起播 payload 移入 player/request，根级公开类型与 title SharedString 契约保持，Debug 脱敏逻辑不变。
- 原请求模块中的队列、起播位置、轨道/字幕元数据和日志脱敏测试随所属逻辑迁移，未更改断言或业务算法。删除页面中的 request 子模块及转发导出；851 个应用测试全部保留并通过。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎测试、2 个边界测试、851 个应用测试、6 个日志测试、3 个文档测试通过；`/tmp/tiny-refactor-selection-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-selection-clippy.log` |
| `git diff --check` | 通过 |

用户暂存 Cargo 文件保持不变。剩余完整验收仍包括 Home 业务状态最终私有化/intent 与只读 selector、Playback/通用组件 props/callback API、其余通知 timer、整体依赖审计及真实窗口/媒体 smoke，目标保持进行中。

## 2026-09-29：Home 内容状态私有化、跨状态 intent 与结果入口

- HomeController 将 feed、library、favorites、search、UserDataState、played_video_versions 和三类 mutation controller 共九个字段设为私有。GPUI 层通过 feed_view/latest_row/latest_items、search_view、favorite_section、library_view 和 pending selector 读取；保留借用数据，render 不克隆完整业务响应。原有回归夹具改为 cfg(test) 专用借用入口，生产代码不存在可写测试接口。
- 收藏、已播放和继续观看操作的冲突判定、当前路由判定、详情/列表目标选择、有效用户数据优先级与组合写入移入 controller。FavoriteIntent/PlayedIntent/ResumeItemAction 生成已有 effect command；页面保留 GPUI 菜单、取消句柄、通知和 snapshot 调度。Played accept/apply 继续分两步，确保成功后先暂停待保存快照，再进行数据回写。
- search/favorites/feed/detail 的 effect result 经 controller 统一提交并合并共享用户数据，保留 request slot/token 与当前账号快照双重校验；过期或重复结果不改数据。Feed 合并直接借用已提交的数据，继续保留 latest IO 上限和退役请求释放逻辑。详情资源存在性仍在 adapter 检查，接受后才处理任务、图片和展示变化。
- 新增 5 个纯 HomeController 边界测试，覆盖搜索账号变化/重复结果、收藏路由与双状态回滚、继续观看按 item 并发/已播放互斥、详情有效用户数据选择，以及 resume/latest 结果拒收和 pending 收藏保护。原有 GPUI 回归测试全部通过，应用测试从 851 增至 856。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎测试、2 个边界测试、856 个应用测试、6 个日志测试、3 个文档测试通过；`/tmp/tiny-refactor-home-boundary-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-home-boundary-clippy.log` |
| `git diff --check` | 通过 |

保留用户暂存 Cargo 文件、其他已有修改与原始 spec。Home navigation 与 DetailBinding 仍允许页面绑定业务写入，需要继续迁移；Playback/通用组件 API、剩余通知 timer、整体依赖审计和真实窗口/媒体 smoke 也未完成，完整目标保持进行中。

## 2026-09-29：Home navigation 私有化与只读详情绑定

- HomeController 的最后一个外露业务字段 navigation 已私有化。Root/Favorites/Back 通过 NavigationIntent 进入 controller，路由/root/title 通过只读 selector 提供；打开普通条目和继续观看改走 OpenDetailIntent，详情类型判断、模型创建、缺少 SeriesId 的拒绝逻辑由 controller 负责。页面只应用 NavigationChange，保留焦点/通知、详情资源挂载和两帧延迟释放。
- 新增带稳定 DetailId 的借用 DetailModelView；生产 DetailBinding 只含不可变 SeriesDetailModel 引用和可变 GPUI 展示/任务句柄，不再暴露可变 DetailController。季/集/源/字幕选择、详情请求开始/完成以及起播请求开始/完成均经 HomeController 写入；重复的选择结果展示处理合并。旧可变绑定仅保留在 cfg(test) fixture 模块。
- 页面播放 runner 保留相同的 loading 门控、当前账号校验、通知字符串和 PlaybackRequest/Event 装配；菜单关闭、任务取消、图片准备均在接受业务结果之后执行。路由返回和详情历史继续复用原模型 ID，但激活 scope 重建，旧请求不能提交。
- 新增 3 个纯 controller 测试，覆盖无效继续观看项不改变路由/请求、返回历史详情复用模型并拒绝旧 token、切换视频源取消旧起播且当前账号继续约束提交。应用测试从 856 增至 859，原 GPUI 导航、滚动、菜单、资源释放和起播测试全部通过。
- 全量测试暴露了引擎已有 HTTP 取消测试的同步竞争：服务端写出 headers 后，客户端可能尚未进入 body read，合法返回 Restart。为保持该测试的严格 cancelled 断言，在 HttpClient 中增加仅 cfg(test) 的 body-read 通知，并让 body 分支等待该同步点。生产网络/解码行为未改变；单独取消测试及完整工作区重跑通过。Clippy 同时要求详情任务插入改用 or_default，已修正。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎测试、2 个边界测试、859 个应用测试、6 个日志测试、3 个文档测试通过；`/tmp/tiny-refactor-home-navigation-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-home-navigation-clippy.log` |
| `git diff --check` | 通过 |

用户暂存 Cargo 文件与原始 spec 保持不变。Home 业务字段现已全部私有，但其余可复用组件 API、通知等剩余 timer、Server catalog/overlay 边界、完整依赖审计以及真实窗口/媒体 smoke 尚未全部完成，目标保持进行中。

## 2026-09-29：通知自动隐藏任务归属与取消

- App/服务器页、Home 和添加/编辑服务器弹窗共用 NotificationQueue 的自动隐藏 runner，移除四处 detached timer。通知条目拥有 NotificationHide scope、RequestSlot 和 EffectHandle；回调先通过 identity/token 校验才删除条目。Home 使用工作区身份，App/弹窗使用本地身份；每个 slot 的独立 owner 防止跨队列或 ID 回绕后的旧回调误删新通知。
- 相同 key 替换、手动关闭、按 scope/key 清理、十条容量淘汰、清空与实体释放都会销毁对应条目并取消计时器。通知文案、key、过滤规则、五秒显示时间、容量和动画不变。公共 UI 只通过队列 accessor 连接宿主，不依赖业务页面类型。
- 保留原有四个队列测试，新增三个测试覆盖账号/owner/ID 回绕拒收、独立超时与替换后重新计时，以及全部移除路径的实际任务取消。实体释放测试在 GPUI update 中执行 drop，确保框架完成释放周期。定向通知测试和全量工作区测试通过，应用测试从 859 增至 862。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎测试、2 个边界测试、862 个应用测试、6 个日志测试、3 个文档测试通过；`/tmp/tiny-refactor-notification-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-notification-clippy.log` |
| `git diff --check` | 通过 |

用户暂存 Cargo 文件及原始 spec 保持不变。其余组件 props/intent API、Server catalog/overlay 边界、完整 scope/依赖审计及真实窗口/媒体 smoke 仍待完成，本批自动化验证不替代真实窗口验收。

## 2026-09-29：Server catalog 边界与同步保存提交

- ServerController 的 catalog、counts、counts_failed 和 counts_refreshed 不再向应用模块开放字段访问。应用启动、侧栏切换、卡片动作和编辑入口通过 server/auto_start_server 只读查询解析当前服务器；快照装配只借用 catalog。原 GPUI 测试的直接修改集中到 cfg(test) fixture API；一处模拟后台计数刷新改用正式的 begin_counts/finish_counts 入口。
- 保存/删除候选改为字段私有的 CatalogChange。应用只注入既有 PersistenceService 的同步保存回调；controller 在回调成功后替换 catalog 并清理计数状态，错误直接返回且不提交。保存提案到提交间仍是同一个同步调用链，不引入异步等待；既有 JSON、全局配置合并、重复服务器处理、错误通知与保存时机不变。
- 新增两个纯 controller 事务测试，验证删除失败保留自动启动项与 pending counts、成功后取消被删服务器的请求，以及编辑失败保留认证快照、成功后旧认证响应被拒收。原认证、图标、编辑、删除、侧栏和持久化回归均通过，应用测试从 862 增至 864。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎测试、2 个边界测试、864 个应用测试、6 个日志测试、3 个文档测试通过；`/tmp/tiny-refactor-server-boundary-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-server-boundary-clippy.log` |
| `git diff --check` | 通过 |

用户暂存 Cargo 文件及原始 spec 保持不变。下一处已确认的 UI 边界是 AddServerDialogState::new_edit 仍直接接收 CachedServer；其表单业务规则、其余 overlay/组件 API、完整 scope/依赖审计及真实窗口/媒体 smoke 尚待完成，完整目标保持进行中。

## 2026-09-29：服务器表单 props、controller 与只读 view

- 新增 server/feature/form。ServerFormController 私有持有编辑目标、协议和提交状态；SelectProtocol/AddressChanged/Submit/SetSubmitting intents 统一处理端口替换、完整 URL 拆分、校验和忙碌门控，并返回字段更新或提交结果。Editor 继续拥有输入文本，controller 不保存重复文本或 GPUI 对象。
- ServerController::edit_form 只投影编辑所需字段，AddServerDialogState::from_props 接收专用 ServerFormProps；移除 UI 内的 CachedServer 构造入口和 ServerDialogMode。标题、按钮文案、协议和提交状态由只读 view 提供；协议控件通过值与 callback 绑定，不再持有弹窗实体。弹窗保留 Editor、通知及字段更新，认证/保存 runner 的调用与 token 门控不变。
- 保留原有地址优先、用户名优先、凭据 trim、URL 解析等六个测试并移入纯模型模块；新增三个 controller 测试及两个 GPUI Editor 联动测试。验证默认/自定义端口、无效 URL 不覆盖字段、编辑初值、忙碌提交不清除旧通知和失败后重试。应用测试从 864 增至 869。
- 检查 src/ui 已无 CachedServer、EmbyClient 或 PlaybackPage 引用；这项结果支持通用 UI 的依赖约束，不代替所有业务组件的逐项接口审计或实际窗口验收。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎测试、2 个边界测试、869 个应用测试、6 个日志测试、3 个文档测试通过；`/tmp/tiny-refactor-server-form-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-server-form-clippy.log` |
| `git diff --check` | 通过 |

用户暂存 Cargo 文件与原始 spec 保持不变。完整目标仍待逐项证明：其余业务组件/overlay/effect 边界、所有请求 scope 与依赖方向，以及真实窗口/媒体流程尚未全部验收。

## 2026-09-29：详情 controls 只读投影与剩余要求审计

- 新增 HomeController 的 DetailControlsVm/DetailActionsVm，统一投影用户数据覆盖、操作禁用、选源/字幕可用性、播放条件、选择索引、标签与起播位置。控制栏保留菜单打开状态、布局、文案格式化和 intent 绑定；数据源/字幕条目借用原数据，不复制完整详情。选择菜单与渲染共用模型可用性判断。
- DetailView/DetailBinding 不再接收 CachedServer/App 查询轨道偏好；Home 的 track_preferences adapter 读取 GPUI 全局，SeriesDetailModel 纯合并按源保存的字幕草稿。删除 detail_user_data_pending 转发方法，调用方直接读取 controller；测试 fixture 的轨道选择也复用正式适配入口，去掉重复规则。
- 新增三个纯 controller 测试，覆盖加载期保留选项但禁用切换/播放、空白 source ID 和缺失条目、共享用户数据覆盖与 pending 收藏回滚、字幕草稿跨版本隔离及保留音轨选择。原 GPUI 字幕菜单、起播和用户数据回归均通过，应用测试从 869 增至 872。
- 新增逐项验收审计，明确 Home 卡片 VM、封面资源适配、Playback 组件、Editor timer 和真实 smoke 五类后续检查。核对锁定 GPUI 源码确认封面 Asset future 在后台 executor 运行；当前引擎 manifest/src 无 GPUI 或应用依赖。审计保留未验证项，不据此宣告整个目标完成。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎测试、2 个边界测试、872 个应用测试、6 个日志测试、3 个文档测试通过；`/tmp/tiny-refactor-detail-controls-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-detail-controls-clippy.log` |
| `git diff --check` | 通过 |

用户暂存 Cargo 文件及原始 spec 保持不变。完整目标仍在进行，后续按验收审计中的具体缺口继续修正并补足实测证据。


## 2026-09-29：Home 卡片、封面适配、Editor caret 与 Playback 组件

- Home 新增纯卡片 VM 和 controller selector。继续观看、普通海报、单集、收藏、详情单集、人物卡片都接收展示值；用户数据优先级由 controller 借用投影。移除卡片内的原始 Emby 条目依赖、重复格式化与展示用完整记录克隆，保留所有尺寸、文案、徽标、hover、进度和点击绑定。六个原纯测试移入 model，新增三个测试覆盖有效用户数据、进度边界及缺失 metadata。该步骤独立通过 875 个应用测试和完整门禁，日志 `/tmp/tiny-refactor-home-cards-tests.log`、`/tmp/tiny-refactor-home-cards-clippy.log`。
- 封面 Asset/本地文件读取/裁剪移至 images/cover，FileImageRepository 实现 CoverImageRequest port。GPUI 在后台执行，App 按路径和尺寸缓存结果，显式清除或 App 退出时回收；保持原成功/失败缓存语义。三个裁剪回归迁移到图片适配器，新增两个测试验证实际 PNG→BGRA、目标尺寸、无效尺寸、重复加载共享结果、尺寸隔离、失败缓存与清除。Home 组件中不再有文件读取或解码。
- Editor caret 改用 EditorCaretBlink RequestSlot/EffectHandle，移除独立 enabled/task_active 标志；focus、活动 deadline、一次完成和 blur/drop 生命周期维持原逻辑，token 校验后才 notify。新增三个 GPUI 测试覆盖失焦后晚到、重新聚焦、不同实体、重复绘制、活动延期及实际任务取消。该 UI scope 不发业务错误通知。
- Playback 新增 TrackMenuVm/Intent，组件接收 VM+选择 callback，保留 Off/外挂标签、stream index 和外挂字幕 URL/codec 快照。通用控制按钮改接收 App 与展示 props；音量与缓存浮层为无页面状态组件。详情统计继续通过 PlaybackDetailSection，页面只组装 backend/window/session 输入。新增一个纯菜单测试，原菜单滚动、长文案、鼠标传播、切换/关闭回归全部通过。
- Playback 剧集卡片改用 session 的 EpisodeCardVm 与独立组件；当前内容长度→已解析源→轨道偏好源、非当前项默认源的文件大小优先级保持原逻辑。纯 metadata formatter 移至 model，标题复用 Arc，身份/图片 tag 借用，虚拟列表只为可见行投影。保留现有文件大小、切集/关闭、滚动、图像和布局回归并扩展 VM 断言。剧集入口条件移入 queue selector；进度提示元素改接收 ProgressHoverPreview，几何投影和输入绑定仍留在展示适配层。

本批最终门禁：

| 命令 | 结果 / 临时输出 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 1478 个引擎测试、2 个边界测试、881 个应用测试、6 个日志测试、3 个文档测试通过；`/tmp/tiny-refactor-card-cover-caret-player-tests.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-refactor-card-cover-caret-player-clippy.log` |
| `git diff --check` | 通过 |

定向本地 HTTP 测试受沙箱端口限制时，已在授权环境重跑通过，没有跳过。用户暂存 Cargo.toml/Cargo.lock、原始 spec 和其他已有工作保持。验收审计 A1–A4 的具体缺口已关闭；完整异步/IO/依赖/flush 审计、依赖检查及真实窗口/原生媒体/重启 smoke 仍待最终证明，完整目标保持进行中。


## 2026-09-29：生产入口、边界门禁与首批原生验收

- 将服务器图标 GPUI Asset 与 FileImageRepository 调用从通用 UI 移入 `images/server_icon_assets`。Picker 绑定层显式传入预览 URL 清除集合，通用 UI 不再读取服务器图标目录或构造 IO 仓库；现有缓存与预览清理/重开回归保留。
- 逐一核对生产 spawn/timer/帧回调与提交路径、设置/主窗口关闭、App/Home release/quit 和 in-flight 重复 flush，结果列入 [边界与验证文档](../../application-ui-boundaries.md)。框架 Asset 缓存与必要的退役资源两帧回收单独解释，不增加重复业务代次。
- 发现同 ID 菜单关闭后立即重开时，旧两帧焦点回调可能提前抢焦点。DropdownFocus slot/token 替换旧的只比较菜单 ID 的判断；两个 GPUI 回归覆盖重开、close、旧 token 和 release。
- 新增 `scripts/check_ui_boundaries.py`：检查 436 个 Rust 源文件、engine manifest 和 Cargo 完整解析图；包括 target/build/dev/间接依赖。九个反例测试验证注释/字面量、cfg(test)、别名、平台和间接依赖边界。源码检查明确是词法 lint，不宣称完整 Rust 名称解析。Cargo metadata 首次补齐缺失平台包后 offline 检查通过，用户暂存 Cargo.toml/lock 未变。
- 首批原生窗口使用生成媒体和 loopback fixture、bwrap 隔离缓存及配置。登录→首页→详情→真实播放/字幕→暂停/seek/恢复→切集失败/重试→返回、开发设置主题/数值步进/close flush、正常退出与用户设置重启恢复已有实际证据；用户模式修改后主窗口关闭同时关闭设置并保存。用户随后明确手动验收由其执行；临时工具与产物已移出仓库，所有本批进程已正常退出/停止。
- 新发现 Settings validation 尚未进入 VM，登记 A6；真实 smoke 未覆盖的项目逐项保留。整体目标仍未完成。

本批门禁与持久证据：

| 检查 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked --quiet` | 通过：1478 engine、2 boundary、883 app、6 logging、3 doc |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过 |
| `git diff --check` | 通过 |
| Python 边界检查器自测 | 9 通过 |
| 源码 + manifest + 解析依赖图 | 通过，436 个 Rust 源文件 |
| `cargo build --locked` | 通过，原生 smoke 实际使用该二进制 |
| 原生证据 | 用户已接管手动验收；本次临时产物移到 `/tmp/tiny-refactor-native-smoke/archive`，不纳入仓库改动 |

本批临时门禁输出：`/tmp/tiny-refactor-boundary-audit-tests.log`、`/tmp/tiny-refactor-boundary-audit-clippy.log`、`/tmp/tiny-refactor-boundary-check.log`、`/tmp/tiny-refactor-smoke-build.log`。最终剩余事项以验收审计矩阵为准，不以本批测试数量或首批 smoke 通过推断全部完成。


## 2026-09-29：按用户指示收敛交付与 Settings 验证投影

停止原生/手动验收，仅继续 spec 的代码修改和自动检查。已将临时 fixture、截图和原生日志移出仓库，保留纯依赖边界检查脚本。

SettingsVm 现在借用逐字段验证状态并公开 editable，controller 以验证投影前后变化决定展示通知。无效文本仍由 Editor 保存，已接受配置保持原精度；没有增加错误样式、改变文案或改动数值步进。既有验证测试通过 VM 断言，新增两种模式下验证状态隔离/恢复、重复无效输入、分类搜索和关闭行为回归。聚焦 `cargo test --locked --lib settings:: --quiet` 12 项通过；全量门禁见后续记录。

最终 render 追踪还需处理上报 mailbox 的锁提交路径，详细调用链见验收审计末尾。该项是源码工作，手动验收已不构成代理任务依赖。


## 2026-09-29：帧外上报与最终自动门禁

`player/page/report_delivery` 使用页面侧 Rc/RefCell 有界 staging；当前帧只接受带原 token/identity 的命令，每批最多一个 foreground continuation，帧结束后才写 worker mailbox。staging 和 worker 共享 Pending 的 Start → Stop/latest Progress 顺序；Stop 清除待发 Progress，接受后拒绝后续报告。continuation 只保留报告与 transport，不保留页面；释放前接受的 Stop 继续提交，执行器 teardown 丢弃 future 时也会先 flush 再关闭 transport。

新增三个回归分别验证 10000 次进度编辑在帧内不触碰 mailbox、仅交付最新进度；页面释放前 Start/Stop 与跨账号拒收；执行器 teardown 的停止回执。既有 10 秒周期、去重、关闭与释放回归已适配 foreground delivery 并通过。播放层 262 项通过。

最终门禁全部通过：`cargo test --workspace --locked --quiet`（1478 engine、2 boundary、887 app、6 logging、3 doc）、Clippy `-D warnings`、fmt check、diff check，以及 437 个 Rust 源文件/manifest/完整解析图检查和九个检查器自测。日志为 `/tmp/tiny-refactor-final-workspace-tests.log`、`/tmp/tiny-refactor-final-clippy.log`、`/tmp/tiny-refactor-final-boundaries.log`。

最终源码核对及用户负责的手动验收范围见 [交付审计](audit.md)。本次代码与自动检查工作完成；不再把历史手工 smoke 清单当作代理后续前置任务。


## 2026-09-30：目录归属、共享绑定与 IO 装配收尾

按目录结构评审建议保留顶层业务组织，以及 HomePage/HomeContent/HomeController、Player session/page 与两种 reporting 所有权。用户已确认本轮一并处理文档和脚本，继续由用户执行真实窗口与媒体验收。

- Detail、Search、Library 专属纯模型分别归入各 feature 的 `model.rs`；SidebarController 归 `home/sidebar/controller.rs`。工作区导航、共享分页、卡片和用户数据覆盖仍归 `home/model`，工作区协调 controller 保留。
- 设置界面归 `settings/view/{dialog,user,development}.rs`，服务器新增/编辑表单归 `server/view/form.rs`。通用 dropdown、数值步进和 toggle 留在 `ui`，设置专属选择器与布局行归 `settings/view/controls.rs`；保留原控件 ID、事件、步进、焦点和保存行为。
- 共享语言 Global/get/apply 与账号轨道偏好绑定集中到 `settings/binding`，纯媒体类型与选源规则仍在 `media`。内部生产消费者直接引用共享所有者，Player 对外 `pub use` 保留；少量内部测试 facade 用 `cfg(test)` 限定。
- Shell 在 `app/auth` 用当前账号快照装配 `HomePorts { browsing, playback }`。Feed、Library、Search、Favorites、Detail 和 Resume/Played mutation 的 GPUI binding 执行注入的浏览 gateway，不再自行构造 EmbyHomeGateway。新增页面级 fake 回归证明 Feed/Library 的成功结果、查询参数和当前失败通知走注入路径。
- `media/gateway::PlaybackSourceGateway` 与 `player/reporting/gateway::PlaybackReportGateway` 独立。EmbyPlaybackGateway 直接实现两者，删除原组合 trait 与 blanket 转发；上报 fake 只实现 report，来源 fake 只实现来源/字幕。
- 图片转换/延迟回收归 `player/image_resources.rs`；Favorites 概览和分类条目分别归 `view/overview.rs`、`view/items.rs`。Home 共用返回按钮、轮播 helper 归 `home/components`。公共播放建页夹具归 `player/page/test_support.rs`，详情内联测试归 `home/detail/tests.rs`；未添加 mod.rs。
- 边界脚本覆盖全部 media、迁移后的 feature 模型、共享 ports；Settings 的纯层采用明确清单，排除 GPUI view/binding。UI 禁止两种 Playback port 及业务 controller，Home binding 禁止具体 gateway 重新装配。检查器新增真实目录分类和拒绝回退测试，并修复 cfg(test) 函数泛型返回值导致词法跳过提前结束的问题。
- 长期架构说明仍为 [应用 UI 模块边界](../../application-ui-boundaries.md)，当前路径、port、生命周期和夹具归属已同步。spec、原始 audit 与逐批 progress 归此重构记录目录；spec 内容保持原样，历史源码路径不追溯改写。

| 命令 / 核对 | 当前结果 |
| --- | --- |
| `cargo test --workspace --locked` | 1478 engine、2 boundary、890 app、6 logging、3 doc，共 2379 项通过，无警告；`/tmp/tiny-ui-followup-tests-final.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-ui-followup-clippy-final.log` |
| `cargo fmt --all -- --check`、`git diff --check` | 通过 |
| `python3 -B scripts/check_ui_boundaries.py` | 464 个生产 Rust 源文件、engine manifest 和 Cargo 完整解析图通过；`/tmp/tiny-ui-followup-boundaries-final.log` |
| `python3 -B -m unittest discover -s scripts -p 'test_check_ui_boundaries.py' -v` | 14 项检查器自测通过；`/tmp/tiny-ui-followup-boundary-tests-final.log` |
| 文档、依赖与暂存区 | 所有 Markdown 相对链接有效；spec 哈希保持原样；Cargo.toml/Cargo.lock 和引擎源码无本轮变更；暂存区条目/内容哈希与开始时一致 |

新增回归 `home::ports::tests::feed_and_library_use_injected_browsing_for_success_and_failure` 通过。来源 port 注入、共享偏好隔离、两种设置视图、服务器表单、报告终态/投递取消及视频图像释放的原回归均随完整工作区测试通过。本轮改动留在工作区，未重新暂存或提交。


## 2026-10-01：目录评审的后续优化

按评审顺序落实交付归档、可持续边界检查、Server 视图、Playback 装配、命名和大文件拆分。原始 spec 内容保持原样；引擎源码和依赖清单未修改，真实窗口与原生媒体验收继续由用户执行。

- 维护中的架构说明和原始 spec/audit/progress、边界脚本及其自测一并纳入版本控制范围，README 提供发现入口。当前说明与播放引擎文档已同步新路径；本地 AGENTS.md 同步媒体、设置与无 GPUI 引擎边界，但它沿用现有 `.gitignore` 规则。
- 每个纯模块自动覆盖同名子目录，Settings 入口的 view/binding 子目录明确排除；应用、引擎、集成测试和 examples 禁止 `mod.rs`。新增真实目录自测覆盖未来 model/controller 子模块、Player 装配约束、图标选择器的 Shell/IO 禁用规则。
- `server/view/icon_picker.rs` 接收只读 VM、catalog、焦点/滚动/Editor/corners 与 callback，负责面板、虚拟网格和图标单元。App 保留 overlay、session、搜索订阅、下载、保存、焦点恢复和预览清理；虚拟列表与意图回调仍验证当前 session/服务器，原尺寸、控件 ID 和忙碌行为保持。
- 新增纯 `player/ports::PlaybackPorts`，Shell 在挂载时提供来源和上报接口；queue/reporting GPUI 绑定不构造具体 gateway。公开 PlaybackPage 构造函数通过 adapter 工厂保持旧契约；同一不可变账号快照服务该页面的两个 port。两种 port 可分别使用 fake；报告 staging、worker、timer 与已接受 Stop 的释放后交付顺序不变。
- Detail 的 `render` 统一为 `view`；Sidebar 绘制归 `sidebar/view`，GPUI 事件与焦点适配归 `sidebar/binding`。移除这些正常 Rust 子模块及 controls、Emby user 模块的冗余 `#[path]`。Player `page/view` 与 `page/render` 保持各自组合和展示辅助职责，Server 的 feature/view 分层保留。
- Detail model 实现拆为 construction/selectors/selection/transitions，仍是唯一状态 owner；829 行原内联测试迁至 model/tests。Home components 拆为 poster/episode/person/images/navigation，入口保留原调用名和测试。开发设置拆为 layout、items/playback、items/cache、controls，仍使用一个 controller 和编辑器资源，原 29 行顺序、文案、控件和 numeric range 保持；分组使用定长数组汇合，结果 Vec 只分配一次。

| 验证 | 结果 |
| --- | --- |
| `cargo test --workspace --locked` | 1478 engine、2 boundary、892 app、6 logging、3 doc，共 2381 项通过，无警告；`/tmp/tiny-ui-directory-tests-final.log` |
| `cargo clippy --workspace --locked --all-targets -- -D warnings` | 通过；`/tmp/tiny-ui-directory-clippy-final.log` |
| `cargo fmt --all -- --check`、`git diff --check` | 通过 |
| `python3 -B scripts/check_ui_boundaries.py` | 481 个生产 Rust 源文件、engine manifest 和 Cargo 完整解析图通过 |
| 检查器自测 | 17 项通过，包括纯层子目录、mod.rs、Player 装配和 picker 归属反例 |
| 迁移核对 | Detail 58 个函数、Home component 26 个函数体保持；设置行描述与展示顺序保持；当前文档链接和源码路径有效 |

新增 `injected_source_drives_queue_failure_and_replacement` 验证注入来源的失败恢复和成功替换 payload；`injected_reporter_delivers_accepted_stop_after_page_release` 验证注入上报的 Start/Stop 顺序、receipt 与释放。原 Server picker、两种设置视图、详情/卡片、queue/reporting 生命周期回归继续通过。
