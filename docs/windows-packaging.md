# Windows x64 可移动目录包

使用 `x86_64-pc-windows-msvc` 构建，生成根目录可直接双击的
`tiny-player.exe`、运行时 DLL、资源、许可证和校验清单。移动时复制整个目录。
配置和缓存仍保存在用户目录；这是程序便携包，不是把用户数据也写入程序目录的模式。

## 构建条件

- Windows x64，Rust MSVC 工具链（本次使用 1.98.1）。
- Visual Studio 2022 C++ x64 工具或 Build Tools，Windows SDK（本次为 10.0.26100.0）。
  SDK 的 `rc.exe` 和 `fxc.exe` 分别用于应用资源和 GPUI release shader。
- **64 位 Python 3.12**。工具依赖使用锁定的 CPython 3.12 wheel。
- 首次准备需要网络，后续可使用 `-Offline`。

```powershell
.\scripts\package-windows.ps1
```

脚本会自动查找已安装的 Python 3.12 x64：Python launcher 列出的解释器、PATH、
标准安装目录，以及本机已有的 Codex 工具运行时。找到后会输出实际解释器路径；
不会自动安装 Python，也不会执行指向 Microsoft Store 的占位命令。
如需指定其他解释器，使用 `-Python` 传入**本机实际存在的 `python.exe` 路径**。
`C:\Python312\python.exe` 不是固定安装位置，不应直接复制使用。

脚本自动定位 Visual Studio 并加载 x64 开发环境，所有下载、SDK 和构建缓存放在
`target/`，不安装全局 Python 包或系统级 MSYS2。不要设置覆盖项目配置的
`RUSTFLAGS`、`CARGO_ENCODED_RUSTFLAGS` 等环境变量。

原生依赖由 `packaging/windows/native-lock.json` 固定 URL、版本和 SHA256：

- BtbN FFmpeg 9.0 分支的 LGPL shared 构建 `n9.0.1-84-g946fcce07b`。
- MSYS2 UCRT64 libplacebo 7.360.1-2 及匹配的 Vulkan、shaderc、色彩管理等依赖。
- libclang 18.1.1，用于项目和 ffmpeg-sys-next 的 bindgen。

Rust 应用仍使用 MSVC。对原生 DLL 的 C ABI，脚本从导出表生成 `.def`，再通过
MSVC `lib.exe` 生成导入库，并正确标记数据导出；不链接 MinGW 静态库，也不引入
MinGW CRT 头文件。FFmpeg 的头文件和 DLL 来自同一归档。生成的 pkg-config 文件
指向项目内 SDK，满足 `build.rs` 的版本检查。

## 使用 cargo run 开发

首次在 Windows 检出项目后，先准备原生依赖和本地 Cargo 配置，无需先构建 release 包：

```powershell
.\scripts\package-windows.ps1 -Mode Prepare
cargo run
```

已经下载过依赖时可用 `-Mode Prepare -Offline`。打包脚本的其他模式也会完成此准备。
准备后可在新的 PowerShell 窗口直接执行 `cargo run`、`cargo check`、`cargo test`。

准备流程生成未纳入 Git 的 `.cargo/config.toml`，让 Cargo 在编译依赖之前就获得
`FFMPEG_DIR`、pkg-config、libclang 和头文件搜索路径，避免 `ffmpeg-sys-next`
错误地回退到未安装的 vcpkg。Windows runner 会为运行程序和测试临时添加 SDK 的
DLL 路径，不修改系统 PATH。不要只在主项目的 `build.rs` 里设置这些变量：
依赖的构建脚本在此之前就可能已运行。

移动源码目录或删除 `target/` 后，请重新运行准备命令。本地配置含机器路径，
不会随 Git 影响 Linux 构建。已有手写 `.cargo/config.toml` 或 `.cargo/config`
时准备流程会提示处理冲突，不会覆盖它们。

## 输出

以下路径中的 `<version>` 由 `Cargo.toml` 中的应用版本自动生成：

```text
dist/
  tiny-player-<version>-windows-x86_64/
    tiny-player.exe
    *.dll
    share/tiny-player/assets/
    licenses/
    build-info/
    manifest.json
    README.txt
  tiny-player-<version>-windows-x86_64.zip
  tiny-player-<version>-windows-x86_64.zip.sha256
```

重复打包会把旧目录重命名为 `.previous-时间戳`，ZIP 更新为新版本。
不要只复制 exe。用户机器无需 Rust、Python、Visual Studio、FFmpeg 安装或 PATH 配置。

## 运行条件与检查

面向 Windows 10 22H2 / Windows 11 x64。GPUI 使用 Direct3D 11；视频处理仍使用 Vulkan。
包内包含 Vulkan loader，显卡 Vulkan 驱动必须由用户系统提供。
默认软件解码，`TINY_HWDEC=auto` 可尝试 Vulkan 硬解并在不可用时回退。
软件解码后的常见 YUV 帧也经过 libplacebo，因此不等于无 Vulkan 需求。

打包程序递归检查 PE 的普通导入、延迟导入和转发导出，拒绝非 x64 二进制或缺失
非系统 DLL。只有明确列出的 Windows 系统 DLL/API set 可不随包携带。
Rust 的 MSVC 运行库静态链接；原生多媒体库动态链接。

```powershell
# 只检查，不打包
.\scripts\package-windows.ps1 -Offline -Mode Check

# 对所有 target 做严格 Clippy 检查
.\scripts\package-windows.ps1 -Offline -Mode Clippy

# 运行测试，包括磁盘缓存并发读写和移动后资源查找
$env:TINY_TEST_LIBPLACEBO = '1' # 可选：在有 Vulkan 驱动的机器启用真实 GPU 测试
.\scripts\package-windows.ps1 -Offline -Mode Test

# 定位单项测试（输出测试日志）
.\scripts\package-windows.ps1 -Offline -Mode Test -TestFilter libplacebo_tone_maps

# 使用已生成的 release exe 重新收集、审计和打包
.\scripts\package-windows.ps1 -Offline -SkipBuild
```

验收时将完整目录复制到带空格或中文的其他路径，从无关工作目录启动，清除开发库
搜索路径，验证界面资源和日志，再测试播放及磁盘缓存。PE 检查不代替显卡实测。
日志默认写入 `%LOCALAPPDATA%\tiny-player\logs\tiny-player.log`，可通过
`TINY_LOG_FILE` 修改；设为空字符串可关闭文件日志。

校验已搬移的目录（包括 manifest 内每个文件的 SHA256），请将 `<version>` 替换为实际包版本号：

```powershell
# 使用上次构建记录的实际解释器路径
$native = Get-Content .\target\windows-x86_64\paths.json -Raw | ConvertFrom-Json
& $native.python packaging\windows\bundle.py --verify "D:\播放器\tiny-player-<version>-windows-x86_64"
```

此流程生成未签名的目录和 ZIP，不生成 installer。原生包版本、来源、构建元数据、
许可证及 Rust 依赖信息随包保存，供后续发布时核对。
