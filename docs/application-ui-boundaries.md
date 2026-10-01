# 应用 UI 模块边界与验证

本文描述当前依赖方向和可重复执行的检查。原始要求见 [重构规范](refactors/application-ui-business/spec.md)，原始交付与用户验收范围见 [历史审计](refactors/application-ui-business/audit.md)。检查通过不替代真实窗口、原生媒体或重启流程。本文随当前源码维护；spec、审计和 [迁移记录](refactors/application-ui-business/progress.md) 归入重构记录目录，历史路径不作追溯改写。

## 所有权与依赖方向

| 层 | 当前入口 | 允许的职责 / 边界 |
| --- | --- | --- |
| Shell | `app.rs`、`app/shell`、`app/intent` | 路由、挂载 token、订阅、窗口/overlay、全局配置和跨 feature 事件。`TinyApp` 持有 controller 与执行资源；调用 gateway/effect，不能把 Emby 响应解析和业务字段重新放回 Shell |
| Server | `server/feature`、`server/view` | Controller 私有拥有 catalog、认证、计数、选择、图标状态；view 接收 ServerCardVm/ServerMenuVm 与 ID callback；新增/编辑表单位于 `server/view/form`，图标选择器绘制在 `server/view/icon_picker`，由 Shell 注入 VM、展示资源及经过 session/服务器校验的 callback；SidebarServer 投影由 `server/sidebar` 提供。`CatalogChange` 同步保存成功后提交 |
| Home | `home/controller`、`home/model` 与 `home/{feed,detail,search,library,favorites,sidebar}` | 工作区 controller 协调导航/详情历史、内容、用户数据覆盖、mutation 和快照；专属 model/controller 归各 feature，Detail model 按构造、查询、来源选择与状态转换拆分子模块但保持唯一状态所有者；SidebarController 位于 `home/sidebar/controller`，GPUI 绑定和组件分别位于 `home/sidebar/binding`、`home/sidebar/view`。共享导航、分页、卡片与用户数据覆盖留在 `home/model`。页面拥有 Entity、滚动、焦点、图片路径/执行资源；读取 selector，发送 intent，回传结果 |
| Playback | `player/session`、`model`、`queue`、`reporting` | session 拥有 source/timeline/controls，queue/reporting 有独立 reducer；Shell 注入账号 snapshot 对应的 `PlaybackPorts { source, reporting }`，页面绑定只执行注入 port，不装配具体 gateway；页面执行 backend command 并绑定 VM/callback。公开页面构造函数通过 adapter 的默认工厂保持原有调用契约。展示帧、字幕、视口、焦点、窗口拖动与 timer handle 属 presentation |
| Settings | `settings/controller`、`settings/model`、`settings/values` | 编辑、校验、分类、搜索与精确设置 snapshot；GPUI Editor 保存未提交文本。完整界面及设置专属控件在 `settings/view`，共享语言/轨道偏好的 GPUI Global/get/apply 在 `settings/binding`；纯值留在 `media`。Shell 注入完整 SettingsSnapshot，持久化桥接在 Shell 适配层。VM 借用逐字段 validation 并暴露 editable；验证投影的变化决定展示通知，不复制 Editor 草稿或改变步进行为 |
| 共享媒体 | `media` | 轨道、语言、队列、偏好值、版本与选择规则以及来源 port；不含 GPUI Global 或运行时 IO。Player 对外 pub use 可保留，内部消费者直接引用共享所有者 |
| 通用 UI | `ui` | Editor、Scrollbar、Tooltip、Dropdown、NumberControl、Toggle 等小型 props、回调、交互状态；不导入 CachedServer、EmbyClient、PlaybackPage、持久化服务或具体 IO 仓库。GPUI Asset 类型可作为展示适配边界；控件不构造仓库、不管理服务器目录 |
| IO ports | `server/feature/gateway`、`home/gateway`、`media/gateway`、`player/reporting/gateway`、`persistence/adapter`、`images` | 返回业务结果或展示图像，不返回 Window/Context/Entity。生产适配器调用现有协议与文件编码；fake 控制成功、失败和乱序 |
| 引擎 | `crates/tiny-playback` | FFmpeg、音频、缓存、Vulkan/libplacebo。应用依赖引擎；引擎及其已解析依赖图不得引入应用或 GPUI |

图片有两类寿命。Home/剧集网络下载由 ImageController 的账号/ItemImage token 与页面执行句柄管理；封面解码和服务器图标的 GPUI Asset 由框架缓存管理。卡片 Asset 是应用缓存；picker 预览另含 session UUID，关闭时由 picker 绑定层传入 URL 清除。视频/字幕图像使用 `player/image_resources` 的现有缓存与两帧延迟 atlas 回收；这些框架适配器不承担业务状态。

## 可执行依赖检查

```sh
python3 -B scripts/check_ui_boundaries.py
python3 -B -m unittest discover -s scripts -p 'test_check_ui_boundaries.py' -v
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --locked --all-targets -- -D warnings
git diff --check
```

依赖检查使用 Python 3.11+，不修改 Cargo.toml/Cargo.lock。默认调用 `cargo metadata --locked --offline --format-version 1`；新环境如缺少平台包，先运行 `cargo metadata --locked --format-version 1` 填充 Cargo 缓存，或将其输出通过 `--metadata path.json` 传入。元数据必须属于当前 workspace，缺失图时失败，不能以跳过图检查代替通过。

检查内容：

- 扫描所有应用/引擎 Rust 源文件，忽略注释、字符串和显式 `#[cfg(test)]` 项；目标平台条件不被忽略。
- 通用 UI 禁止业务实体、gateway/persistence port 和具体 IO 仓库；纯 controller/model 禁止 GPUI、具体 IO 适配器与直接阻塞 IO/线程入口。受约束模块清单在脚本的 `PURE_GLOBS`，每个纯模块自动覆盖同名子目录；Settings 入口的 view/binding 子目录保持显式排除。新增纯模块时同步维护。所有应用与引擎模块（包括测试）禁止 `mod.rs`。
- 请求计数标识 `generation` 仅允许在 `effects.rs`；业务 revision 与媒体/引擎自身计数不属于这个规则。
- 检查 engine manifest 的普通/build/dev/平台依赖及 workspace/package 别名；再遍历 Cargo 解析后的完整依赖图，包含中间依赖与目标条件，拒绝 GPUI、gpui_platform 或应用反向依赖。

源码部分是词法依赖 lint，不是完整 Rust 名称解析器，不证明被调用函数没有副作用，也不分析宏展开或任意跨模块重导出。类型/字段可见性、编译、controller 回归和以下调用链审计仍须保留。检查器自测同时用临时真实目录调用正式 `check()` 验证 `media`、feature model、业务 view/runtime binding 的分类及两种 Playback port 的 UI 禁用规则；还注入直接/别名/嵌套依赖、平台条件、间接依赖与非法代次，验证规则能够拒绝违规，而不只验证当前树能够通过。Home binding 额外禁止重新装配具体浏览/播放 gateway，Player page 绑定禁止重新装配具体播放 gateway；测试专属装配保留在 test_support。`settings/view` 和 `settings/binding` 是允许 GPUI 的功能适配层，纯层清单只包含 settings 的 controller/model/values/memory_budget 和入口。

## 文件组织约定

功能入口 `<feature>.rs` 负责模块声明和必要导出，子目录按 model/controller/effect/binding/view 的实际职责组织。Detail 的组件统一使用 `view` 命名；Sidebar 的 GPUI 输入和焦点适配统一使用 `binding`。Player 的 `page/view` 组合整页，`page/render` 保留状态展示、几何和帧请求辅助，两者职责不同。

Home 共用组件在 `home/components/{poster,episode,person,images,navigation}`，原对外调用名由入口导出。Detail 状态和约束仍由 `SeriesDetailModel` 拥有，实现拆为 `model/{construction,selectors,selection,transitions}`，原测试归 `model/tests`。开发设置页保留单个 controller 和编辑器资源；`development/layout` 负责布局，`development/items/{playback,cache}` 保持原行顺序，`development/controls` 负责输入与类型化选择器。

## 生产异步入口核对

2026-09-30 在目录与 port 收尾后复核源码中的 spawn/background_spawn/spawn_in、timer、on_next_frame、release/quit 入口逐一核对；测试模块不计入。网络/文件任务的返回值不直接写入业务状态；提交入口先验证当前请求，随后才处理结果数据、图像、通知和保存。用户意图触发的乐观更新仍由相应 controller 管理。

| 入口 | 提交检查 / 生命周期 | 错误去向 |
| --- | --- | --- |
| `app/auth` | ServerController::finish_authentication 校验 ServerAuth token 和凭据 snapshot；替换/取消/drop 取消 auth handle | 当前登录通知；Ignored 无通知 |
| `app/dialogs` | 当前 dialog entity + finish_submission 的 ServerSave token；关闭/替换取消 save handle | 当前表单通知；旧 dialog 不提交 |
| `app/item_counts` | finish_counts 校验 ServerCounts 与服务器认证 snapshot；编辑/删除清理该 handle | 当前计数失败状态；保存失败走统一持久化通知 |
| `app/server_icon_picker` | finish_icon_download/finish_icon_save 校验 ServerIcon 与目标 snapshot；关闭、改目标、释放取消 | picker 当前错误；失败保留原图标 |
| `home/feed/binding` feed | complete_feed 的 workspace + 对应 HomeSnapshot/UserViews/ResumeItems/LatestItems token；refresh 失效旧请求；Latest 最多四项，旧结果仅归还名额 | Home 对应通知 key；过期结果无通知 |
| `home/feed/binding` cached followup | HomeCachedImages/HomeNetworkRefresh slot + workspace；取消 timer handle；完成 commit 后才加载 | 无独立业务错误 key；后续请求各自处理 |
| `home/image_effects`、`player/page/episodes` 图片 | ImageController::finish_job 检查 ItemImage token/workspace/尝试；实体释放取消 continuation；同 workspace 可复用完成缓存 | 失败进入有界重试状态，不弹业务错误；Ready 才 notify |
| `home/search/binding` | complete_search 校验 query/paging token/workspace/user-data revision；输入变化取消 | search initial/load-more key |
| `home/library/binding` | complete_library 校验 view/sort/paging token/workspace；请求句柄属该库展示资源 | library initial/refresh/load-more key |
| `home/favorites/binding` 分页 | complete_favorites 校验分类/paging token/workspace/revision；refresh/mutation 取消 | favorites 分类与阶段 key |
| `home/favorites/binding` mutation | complete_favorite 校验 FavoriteMutation/workspace，成功提交或精确 rollback；替换/drop 取消 | 原动作的 scope/key；只在接受后保存 |
| `home/resume_actions` | complete_resume_action 校验条目 token/workspace；按条目持有 handle | HOME_RESUME_ACTION_NOTIFICATION_KEY |
| `home/detail/actions` | accept_played 先验证 PlayedMutation/workspace，整剧额外校验 DetailActivation/季；再 apply_played_completion | 动作携带的 scope/key |
| `home/detail/loading` | complete_detail 校验 item/resource/季/token/workspace/revision；导航到历史立即失效和取消 | 详情资源 key；ResumeSources 保留原静默策略 |
| `home/detail/launch` | complete_detail_playback 校验当前 activation、选中条目/版本、token、workspace；选源/离开详情取消 | DETAIL_PLAYBACK_NOTIFICATION_KEY；旧响应不打开播放器 |
| `home/playback` 返回刷新 | HomePlaybackRefresh token/workspace + stop receipt；有限次轮询，替换/drop 取消 | 无额外通知，只有当前成功停止才启动刷新 |
| `home/layout` | HomeResize timer + HomeDashboardWarmup 两帧 token；新 resize/内容变化失效；弱 entity | 无业务错误；布局可见变化才 notify |
| `player/page/queue` | QueueController::complete 校验 PlaybackQueue/workspace；返回、失败、drop 取消 | queue error VM；当前失败恢复暂停/结束语义 |
| `player/page/backend_events` | complete_poll 校验 PlaybackBackendPoll/workspace，并重检 backend/error/cache；结束/失败取消 | 无额外通知；需要继续轮询才 notify |
| `player/page/reporting` | accept_periodic 校验 PlaybackReportTimer/workspace；关闭取消周期 handle | 报告 worker 的静态日志/receipt |
| `player/page/reporting/delivery`、`player/reporting/effect` | 帧内先提交到 Rc/RefCell 有界 staging，每批一个 foreground continuation 帧外投递；不持有页面，保留已接受 Stop；执行器取消时 Drop 最终交付。发出与执行时 command.accepts，完成时 command.finish 校验各 Start/Progress/Stop token 与账号；有界 mailbox；释放后只完成已接受报告 | 静态日志与终态 receipt；不写退役页面 |
| `player/page/timers` | 四种 PresentationTimer 经统一 token/workspace；close/drop/替换取消 | 无业务错误；实际可见状态变化才 notify |
| `ui/notification` | NotificationHide token、owner、ID 校验；替换/移除/清空/驱逐/drop 取消 handle | 仅移除当前通知 |
| `ui/editor` | EditorCaretBlink token + 单调 deadline；blur/drop 取消，活动延长等待 | 无业务错误；当前一次到期才 notify |
| `ui/dropdown` | DropdownFocus token；每次 show 替换，close 失效，弱 entity；第二帧 commit 后聚焦 | 无业务错误；旧开启动作不能影响同 ID 重开 |
| `persistence` debounce/write/drain | 独立 timer/write Persistence slot；资源账号隔离；snapshot 指针区分更新；write token 提交；drain 只等待同一串行 worker | 配置通知/释放时静态日志；Home 保存失败保留 dirty |

以下框架回调不携带远程业务结果：Home 虚拟列表的下一帧自动分页会重检当前 route 与 controller 的加载条件，然后通过正式 intent 产生新的业务 token；播放 viewport/进度 bounds observer 用弱 entity 更新当前展示几何，仅变化时通知；详情/视频/字幕的两帧回调仅保留和释放退役资源，不能取消必要的回收。GPUI Asset 的完成写回原缓存项，清除后不会写入替代项。`logging` 的唯一工作线程消费进程级有界日志缓冲，不接触页面或 feature state。

## 持久化关闭路径核对

| 场景 | 当前调用链 | 自动回归证据 |
| --- | --- | --- |
| 设置窗口关闭 | TinyApp 的 on_window_closed → flush_persistence → flush_all | `automatic_save_coalesces_edits_and_close_flushes_the_latest_values` |
| 主窗口关闭/根实体释放 | on_release → save_pending_cache_on_release → flush_all；关闭设置子窗口 | `closing_main_window_closes_settings_and_flushes_pending_changes`、`pending_settings_are_written_when_the_app_is_released` |
| 应用退出 | TinyApp on_app_quit 返回 flush_all 的 drain；HomeContent 同时提交工作区最终 snapshot 并返回 drain | `application_quit_flushes_each_config_dirty_reason_as_one_latest_snapshot`、`application_quit_captures_invalidated_home_and_waits_for_final_write` |
| Home 释放 | finish_home_snapshot_saves 合并轨道偏好/必要最终 snapshot → flush(workspace) → drain，释放回调 detach 等待句柄 | `close_flushes_pending_track_choices_after_an_older_save` |
| 同一写入中重复 flush | in_flight 与 latest 同 snapshot 时不再排队；更新版本被单独串行写入 | `repeated_flush_while_writing_does_not_duplicate_or_retry_the_same_write`、`edits_during_write_survive_old_completion_and_flush_in_order` |
| 失败/乐观数据 | 失败保持 dirty，下一次 schedule/显式 flush 重试；suspend 禁止写入未确认快照，owner 最终重新提交 | `failed_home_write_is_retained_for_explicit_flush_without_a_retry_loop`、`suspended_home_does_not_write_on_settings_close_and_final_snapshot_flushes_once` |

FilePersistence 仍调用原 storage/Home cache 编码器，未改变 JSON、身份/版本判断、原子 rename、文件路径和权限。配置写入保持事件/timer/关闭路径中的同步事务；Home IO 在后台串行执行。锁仅用于短 coordinator 操作，写盘前已释放；render 不调用这些保存入口。用户已接管手动验收；本次代理交付以 spec 重构代码和自动检查为范围。


## 最终 render 与 Shell 核对

Shell 的 auth/dialogs/counts/icon picker 回调只负责执行 feature command、回传受 token 检查的结果及协调 overlay/窗口/持久化；实际协议在 ServerGateway。AppIntent 先检查挂载 token，再向 Home/Playback 转发事件。Home/Playback 业务字段没有回流到 Shell；配置 snapshot 与目录 proposal 的同步事务在事件/关闭路径执行。

App render 中 `finish_server_reorder(false)` 只取消预览，不进入 CatalogChanged 保存分支；Home render 只读 selector 并绑定意图，帧后自动分页会重检当前 route 与分页状态。Settings 缓存目录在构造时解析，render 不查询文件。封面/图标及 GPUI ImgResourceLoader 属后台 Asset 加载；视频/字幕的 presenter 轮询和帧转换继续使用现有引擎展示适配器。

检查生产代码（去除测试项）后，App/Home/UI/Playback page 的直接文件读写仅见 Home cache 编码器，由 Persistence adapter 调用；render 不调用该编码器。上报链 `render → poll_backend → reporting transition` 已改成只写本线程有界 staging，后续 foreground continuation 才取得 worker mailbox 的短锁，网络 IO 始终在报告 worker。staging 与 worker 共用 Start/latest Progress/Stop 的优先级规则；最多一个待执行 continuation，页面释放不会取消已接受 Stop，也不会保留页面 entity。

卡片 selector 仅复制展示字符串/ID；详情控制读模型借用 sources、subtitle refs 和用户数据，播放帧/标题使用 Arc/SharedString，轨道菜单只在打开时创建选项。请求完成的 notify 在 token 接受和可见变更判定后执行；配置保存完成只更新 dirty/error，Home 隐藏 dashboard 与 resize/warmup 继续由独立布局状态控制。

用户已明确手动验收由其执行。本次交付完成重构代码和自动门禁，不将截图或临时原生 fixture 作为源码产物。

## 目录归属与公共夹具

顶层继续按业务功能组织，不建立全局 domain/application/infrastructure/presentation 四套横向目录。HomePage、HomeContent 和 HomeController 分别保留页面容器、GPUI 资源、业务状态所有权；工作区 controller 与 feature controller 不机械合并。Player 的 session、page、reporting policy 和 page/reporting delivery 也保留各自生命周期。

Home 的工作区返回按钮、轮播 track helper 归 `home/components`；feature 不从另一个页面借组件。Favorites 的概览和分类条目分别位于 `home/favorites/view/overview.rs` 和 `items.rs`。`player/image_resources` 管理图像转换与回收，`player/page/presentation` 管理页面展示状态。

通用控件留在 `ui/dropdown.rs`、`ui/number_input.rs`、`ui/toggle.rs`，解码/语言选择与磁盘容量行留在 `settings/view/controls.rs`。设置界面测试随 user/development view，服务器表单测试随 `server/view/form`。各功能只建立有实际职责的文件，不强制 model/controller/effect/binding/presentation 全部对称。

播放器页面共享内存夹具位于 `player/page/test_support.rs`，上报、字幕、鼠标与后端交互测试不再依赖 episodes 测试模块。详情组合测试位于 `home/detail/tests.rs`，入口只保留模块声明和绑定 API。所有 Rust 模块使用 `<module>.rs` 与同名子目录，不使用 `mod.rs`。


## 当前自动验证记录

2026-10-01：`cargo test --workspace --locked` 共 2381 项通过（1478 引擎、2 边界、892 应用、6 日志、3 文档），严格 Clippy、格式和 diff 检查通过。依赖门禁扫描 481 个生产 Rust 源文件并验证引擎 manifest 与完整解析图；17 项检查器自测通过，包含纯层子目录、禁止 mod.rs、运行时绑定排除和违规反例。页面级 fake 覆盖 Home 浏览注入、Playback 来源失败/成功替换和注入上报的释放后 Stop 交付；两种播放 fake 分别只实现各自 port。

详情见 [迁移记录](refactors/application-ui-business/progress.md)。上述结果不替代用户的真实窗口与原生媒体手动验收。
