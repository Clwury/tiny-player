# Tiny Player

[English](README.md) | 简体中文

Tiny Player 是使用 Rust 和 GPUI 构建的原生 Emby 桌面客户端，面向 Linux 与 Windows。播放器使用 FFmpeg 解码，通过 Vulkan 和 libplacebo 完成视频渲染与色彩处理。

## 功能

- **多服务器管理**：添加、编辑和切换 Emby 服务器，显示服务器图标及媒体数量。
- **媒体库浏览**：电影与剧集海报、搜索、收藏、继续观看，以及季和单集选择。
- **影片详情**：展示影片 Logo、简介、评分、时长、上映日期、分辨率与动态范围信息。
- **播放与选轨**：选择视频版本、音轨和字幕，设置语言偏好，同步播放进度。
- **视频处理**：软件解码、Vulkan 硬件解码、Linux VA-API 回传或 Vulkan GPU 映射，以及 HDR 色调映射；硬解能力取决于显卡、驱动和视频编码。
- **播放体验**：全屏、倍速、字幕位置调整、播放信息面板，以及可配置的内存和磁盘缓存。
- **手柄播放快捷操作**：标准手柄按键支持暂停、跳转、音量、倍速、字幕位置调整和返回详情。
- **播放时保持唤醒**：播放视频时自动请求禁止自动锁屏、屏幕节能与睡眠；暂停、缓冲导致暂停、播放结束或关闭播放器时自动释放请求。

## 播放页手柄操作

播放页自动检测手柄，只有播放窗口处于活动状态时才接收输入。首次操作的手柄取得控制权，断连后可切换其他手柄。Steam Deck 可将 Tiny Player 添加到 Steam，并在 Steam Input 中使用标准手柄布局。

| 按键（Deck / Xbox 布局） | 操作 |
| --- | --- |
| A | 播放 / 暂停 |
| B | 关闭选集列表、音轨/字幕或缓存菜单、播放信息或快捷键面板；没有面板时返回详情 |
| ← / → | 后退 / 前进 5 秒 |
| ↓ / ↑ | 音量降低 / 提高，每次 2 个百分点 |
| LB / RB | 后退 / 前进 60 秒 |
| X | 切换静音 |
| Y | 切换播放信息 |
| Menu / Start | 切换全屏 |
| View / Select | 显示 / 隐藏播放控制栏 |
| LT / RT | 降低 / 提高倍速 |
| L3 | 恢复 1 倍速 |
| R3-↑ / R3-↓ | 按住 R3，上移 / 下移字幕 |

L3 / R3 分别表示按下左 / 右摇杆，箭头表示方向键。播放页快捷键面板也提供完整手柄按键说明；收到手柄按键输入后，本次播放页会将手柄说明显示在键盘说明上方。

跳转和音量支持长按连发，其余操作每次按下触发一次。窗口失焦或离开播放页时停止接收输入，手柄操作后控制栏保留三秒。B 可关闭已打开的菜单；菜单条目选择和媒体浏览仍沿用现有鼠标/键盘交互。键盘快捷键保持原有映射。

Linux 源码构建还需要 libudev 开发包（Ubuntu 的 `libudev-dev`、Arch 的 `systemd-libs`），用户会话需要具备手柄 `/dev/input/event*` 设备的访问权限。

## 应用截图

![Tiny Player 影片详情与剧集选择](docs/tiny-player-1.png)

![Tiny Player 视频播放与控制面板](docs/tiny-player-2.png)

## 系统要求

| 平台 | 运行要求 |
| --- | --- |
| Linux 预编译包 | x86_64 GNU/Linux，glibc 2.39 或更新版本，Wayland 或 X11 桌面会话 |
| Windows 便携包 | Windows 10 22H2 / Windows 11 x64，支持 Direct3D 11 的显卡与驱动 |
| 视频处理 | 可用的 Vulkan 显卡驱动；使用软件解码时，视频处理仍需要 Vulkan |

Linux 包还需要与 Ubuntu 24.04 或更新版本兼容的宿主 `libva.so.2`、`libva-drm.so.2` 和 `libdrm.so.2`。需要对应系统驱动和设备访问权限；可用 `TINY_VAAPI_DEVICE=/dev/dri/renderD128` 指定设备。用户设置的“自动”在 Linux NVIDIA 上按 Vulkan → VA-API 映射 Vulkan → 软件解码尝试，其他 Linux GPU 交换前两项；Windows 保持 Vulkan → 软件解码。开发设置保留“强制 VA-API（回传内存）”，其“自动”也在软解之前保留回传候选；环境变量 `TINY_HWDEC=vaapi` 和 `auto-copy` 分别选择这两种开发模式。详见[解码设置](docs/settings.md)和[真机验证](docs/playback-engine.md)。
性能路径选择“强制 VA-API（映射 Vulkan）”，或设置 `TINY_HWDEC=vaapi-vulkan`，通过 DMA-BUF 直接映射 NV12/P010 GPU 帧，省去解码 YUV 帧的 CPU 回传与上传。需要驱动支持 Vulkan 外部内存、DRM modifier 和同步；强制模式失败明确报错，可在开发设置切回回传模式。最终 BGRA 输出仍回读到 CPU 供 GPUI 显示。

Linux 预编译包包含 FFmpeg、libplacebo 及部分依赖，系统还需提供 Vulkan loader、ALSA 及音频配置、Fontconfig、FreeType、字体、XKB 键盘数据、系统 CA 证书，以及与 Ubuntu 24.04（GCC 13）或更新版本兼容的 GCC/C++ 运行库。glibc 2.39 是预编译包的运行基线，其他方式构建时以构建环境和依赖要求为准。

Windows 便携包包含多媒体依赖与 Vulkan loader，显卡的 Vulkan 驱动由系统提供。运行便携包无需安装 Rust、Python、Visual Studio 或 FFmpeg。

Linux 的播放抑制功能需要 `xdg-desktop-portal` 及实现 Inhibit 接口的桌面门户后端。桌面拒绝请求时会记录日志，播放仍继续。手动锁屏与用户主动触发的睡眠由系统控制。

## 安装

### Arch Linux

使用仓库中的 [PKGBUILD](PKGBUILD) 构建并安装：

```sh
sudo pacman -S --needed base-devel git
git clone https://github.com/Clwury/tiny-player.git
cd tiny-player
makepkg -si
```

`makepkg` 会安装声明的构建和运行依赖。安装后可从桌面应用菜单启动，也可运行：

```sh
tiny-player
```

### Linux 预编译包

适用于 **x86_64（64 位 Intel/AMD）GNU/Linux，要求 glibc 2.39 或更新版本**。前往 [GitHub 最新发布页](https://github.com/Clwury/tiny-player/releases/latest)下载 `tiny-player-<version>-linux-x86_64.tar.gz`。

解压完整归档后运行。请将 `<version>` 替换为所下载包的实际版本号：

```sh
tar -xzf "tiny-player-<version>-linux-x86_64.tar.gz"
./tiny-player/bin/tiny-player
```

也可安装到当前用户目录，无需 `sudo`：

```sh
./tiny-player/install.sh
```

安装位置固定为 `~/.local/tiny-player.app`，同时创建 `~/.local/bin/tiny-player` 启动链接和桌面菜单项。更新时关闭应用，将新版解压到新目录，再直接运行其中的安装脚本，无需传入参数。脚本自动替换应用、更新启动链接、桌面入口和图标，更新失败时恢复原安装，用户配置与缓存保留。

需要自行生成预编译包时，在 x86_64 Linux 主机上准备 Docker，并在仓库根目录运行：

```sh
./scripts/package-linux.sh
```

默认复用同一已审计 payload，同时将 tar.gz、AppImage、各自的 SHA256 校验文件和构建清单输出到 `dist/`。更多信息见 [Linux 打包说明](docs/linux-packaging.md)。

#### AppImage

也可从同一发布页下载 `tiny-player-<version>-linux-x86_64.AppImage` 及其 `.sha256` 文件。请替换为实际版本号：

```sh
sha256sum --check "tiny-player-<version>-linux-x86_64.AppImage.sha256"
chmod +x "tiny-player-<version>-linux-x86_64.AppImage"
./tiny-player-<version>-linux-x86_64.AppImage
```

无 FUSE 时使用解压运行模式：

```sh
./tiny-player-<version>-linux-x86_64.AppImage --appimage-extract-and-run
```

静态 musl runtime 无需 `libfuse2`，但正常挂载 AppImage 仍需要可用的 FUSE。应用仍要求 **glibc 2.39 或更新版本**，并依赖上述宿主 Vulkan loader/驱动、音频配置、字体、Wayland/X11 会话与桌面 portal 支持。软件解码仍需要 Vulkan 渲染；AppImage 不包含显卡驱动，也不绕过 portal 要求。

不保证双击启动、自动安装桌面菜单或自动更新。更新时关闭 Tiny Player，再替换 AppImage 文件。AppImage 与 tar.gz 共用原有用户配置与缓存路径，不将配置存放在 AppImage 旁。

### Windows 便携包

适用于 **Windows 10 22H2 / Windows 11 x64（64 位 Intel/AMD）**。前往 [GitHub 最新发布页](https://github.com/Clwury/tiny-player/releases/latest)下载 `tiny-player-<version>-windows-x86_64.zip`。

完整解压 `tiny-player-<version>-windows-x86_64.zip`（`<version>` 为所下载包的实际版本号），双击目录中的 `tiny-player.exe`。移动应用时保留整个目录，包括 DLL 和 `share/` 资源。配置与缓存保存在 Windows 用户目录中。

自行构建需要 Rust MSVC 工具链、Visual Studio 2022 C++ x64 Build Tools、Windows SDK，以及 64 位 Python 3.12。在仓库根目录的 PowerShell 中运行：

```powershell
.\scripts\package-windows.ps1
```

脚本准备原生依赖并生成 `dist/` 下的应用目录与 ZIP。详细环境准备、离线构建与检查方法见 [Windows 打包说明](docs/windows-packaging.md)。

### 从源码运行

工作空间包含根包 `tiny-player` 应用和 [`tiny-playback` 播放引擎](crates/tiny-playback)。
引擎不依赖 GPUI，通过普通 Rust 类型输出视频与字幕像素；GPUI 图像适配留在应用中。
播放页面与 Emby 集成保留在应用中，模块归属与构建细节见[播放引擎边界](docs/playback-engine.md)。
运行 `cargo test --workspace --locked` 测试两个 crate，或使用
`cargo test -p tiny-playback --locked` 单独测试引擎。

应用 UI 的职责划分和可重复执行的边界检查见[应用 UI 模块边界](docs/application-ui-boundaries.md)。
原始规范、交付审计和迁移记录保存在 `docs/refactors/application-ui-business/`。

克隆仓库后，Linux 开发环境需要：

- Rust stable、C/C++ 构建工具、Clang/libclang、pkg-config。
- FFmpeg 8.1 或 9.x、libplacebo 7.x，以及相应的开发头文件。
- Vulkan 头文件、loader 和显卡驱动，以及 ALSA、Wayland/X11 等依赖，完整列表见 [PKGBUILD](PKGBUILD)。

在仓库根目录运行：

```sh
cargo run --locked
```

构建优化版本：

```sh
cargo build --release --locked
./target/release/tiny-player
```

Windows 开发环境在满足上述 Windows 构建条件后，先准备依赖与本地 Cargo 配置：

```powershell
.\scripts\package-windows.ps1 -Mode Prepare
cargo run --locked
```

## 播放操作与快捷键

以下快捷键在播放页面获得键盘焦点时生效：

右上角缓存速度右侧的键盘图标可显示 / 隐藏键盘与手柄快捷键说明，信息图标可显示 / 隐藏播放信息。两个面板在同一位置显示，切换时自动关闭另一个；快捷键以半透明主题色背景的斜体标签显示，组合键用 `-` 连接，同一操作的多个按键各用独立标签，标签之间用 `/` 分隔，说明跟随界面语言。

说明内容无需滚动时，面板和右上角图标区域的滚轮可调整音量；内容超出可视高度时，面板内滚轮只滚动说明，滚到边界也不会改变音量。

| 按键 | 操作 |
| --- | --- |
| `Space` / `P` | 播放 / 暂停 |
| `F` | 切换全屏 |
| `Esc` | 优先关闭选集列表；否则全屏时退出全屏，非全屏时返回详情页 |
| `←` / `→` | 后退 / 前进 5 秒 |
| `↓` / `↑` | 后退 / 前进 60 秒 |
| `j` / `Shift` + `J` | 循环切换下一条 / 上一条字幕轨道（包含关闭） |
| `v` | 显示 / 隐藏字幕，保留当前字幕轨道 |
| `#` | 循环切换音频轨道（包含关闭，通常为 `Shift` + `3`） |
| `<` / `>` | 播放列表上一集 / 下一集（通常为 `Shift` + `,` / `.`） |
| `Enter` | 播放列表下一集 |
| `F8` | 显示 / 隐藏播放列表（选集列表） |
| `9` / `/` | 音量降低 2 个百分点 |
| `0` / `*` | 音量提高 2 个百分点 |
| `M` | 静音 / 取消静音 |
| `[` | 播放速度除以 1.1 |
| `]` | 播放速度乘以 1.1 |
| `{` / `}` | 播放速度减半 / 加倍（通常为 `Shift` + `[` / `]`） |
| `Backspace` | 恢复 1 倍速 |
| `R` / `T` | 上移 / 下移字幕，每次为视频显示高度的 1% |
| `I` | 显示 / 隐藏播放信息面板 |

倍速范围为 **0.25–4 倍**。音量快捷键也支持数字小键盘的除号与乘号；持续按住音量或倍速调整键可重复调整。长按方向键可连续跳转，左右键每次跳转 5 秒，上下键每次跳转 60 秒。播放 / 暂停、全屏、静音、字幕或播放列表显示、轨道和上下集切换每次按下仅触发一次。播放列表首集和末集不会循环切集。

使用快捷键或菜单切换字幕、音轨成功后，返回按钮右侧以主题色显示“字幕：轨道名称”或“音轨：轨道名称”，2 秒后自动消失；连续切换会更新名称并重新计时。关闭轨道时显示“关闭”。

播放画面上的鼠标操作：

| 操作 | 效果 |
| --- | --- |
| 左键双击画面 | 切换全屏 |
| 右键单击画面 | 播放 / 暂停 |
| 滚轮上 / 下滚动 | 提高 / 降低音量 |
| 点击或拖动进度条 | 跳转到指定播放位置 |

暂停时跳转会显示目标位置的画面，并保持暂停；跳转到视频末尾时显示最后一帧并结束加载。恢复播放后从当前位置继续。

全屏时，鼠标在播放区域任意位置移动都会显示控制面板；停止移动 1 秒后自动隐藏。悬停在控制面板或右上角图标区域、拖动进度条或打开选集列表时保持显示。

## License

本项目采用 **GNU General Public License v3.0 或更新版本（GPL-3.0-or-later）**，完整条款见 [LICENSE](LICENSE)。第三方依赖遵循各自的许可证。

## 致谢

感谢以下项目与应用提供的基础能力、实现参考和设计启发：

- [Zed / GPUI](https://github.com/zed-industries/zed)
- [gpui-kit](https://github.com/longbridge/gpui-kit)
- [Tsukimi](https://github.com/tsukinaha/tsukimi)
- [mpv](https://github.com/mpv-player/mpv)
- [Lenna](https://lennaapp.github.io/)

图标来源于 [离歌图标库（lige47/lige_icon）](https://github.com/lige47/lige_icon)
