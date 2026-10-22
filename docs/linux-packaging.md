# Linux 预编译包

目标为 **x86_64 GNU/Linux、glibc 2.39 或更新版本**。整个程序及随包原生库在
Ubuntu 24.04 容器中编译；不使用构建宿主机的 FFmpeg、libplacebo 或 glibc。运行时使用目标系统的 glibc。
glibc 2.39 是发布基线，包内所有 ELF 的实际版本需求均不得高于它。

## 构建

需要能够运行 Linux amd64 容器的 Docker、网络连接以及充足的磁盘空间。
首次构建需要下载工具链、原生源码和 Cargo 依赖。

```sh
./scripts/package-linux.sh
```

默认并发数为 4，可用 `BUILD_JOBS=8 ./scripts/package-linux.sh` 调整 Cargo 并发。
`CONTAINER_ENGINE` 可指定兼容 Docker 命令行的引擎，`TINY_LINUX_IMAGE` 可修改镜像名。
构建缓存保存在 `target/linux-x86_64-glibc2.39/`，源码以只读方式挂入容器。
容器以 `/build` 为工作目录，通过 `--manifest-path /src/Cargo.toml` 构建，确保 Cargo
使用镜像内的原生依赖路径。本机开发准备脚本生成的 `.cargo/config.toml` 中的强制环境
配置留在源码目录，容器打包及 CI 检查从独立工作目录运行。

打包入口只执行 release 构建、依赖收集、ELF 检查和打包；开发检查单独运行。
默认复用同一已审计 payload，同时输出 tar.gz 与 AppImage，不重复构建应用或改变宿主库排除策略。
本地构建的依赖版本由 `Cargo.lock`、`packaging/linux/build-native.sh` 和 `build-ffmpeg.sh` 固定：
FFmpeg 9.0.2、libplacebo API 365 开发快照
`8c5b8d2fe001336f807a662394d90af396755f20`、Vulkan-Headers 1.4.363；Rust 为 1.97.0。
该 libplacebo 提交包含 `vkGetDeviceQueue2` 及外部设备队列创建 flags 导入修复，
用于避免 FFmpeg 9 与 RADV 内部同步队列组合在首次渲染时提交空 `VkQueue`。
它是固定源码快照，不是稳定发布标签；包内 `build-info/libplacebo.commit` 和
`libplacebo.version` 分别记录实际提交与构建版本。
Ubuntu 安全更新随构建时的仓库更新，镜像不承诺逐字节可复现。

播放页手柄输入使用 gilrs。构建镜像安装 `libudev-dev`，`bundle.py` 自动收集
libudev 及其链接依赖并执行相同的 ELF 审计；运行时由用户会话提供输入设备访问权限。

## GitHub Actions 发布

推送到 `master` 或在 `master` 上手动运行时，`.github/workflows/release.yml` 执行相同的
完整发布流程：从根 `Cargo.toml` 读取版本，以 `v<version>` 为标签创建 Release。
触发发布前需修改应用版本并同步提交 `Cargo.lock`；
已有标签会使检查失败，不覆盖已有发布。检查通过后，两端并行构建、运行测试与 Clippy，
完成 ELF/PE 审计并上传构建产物；只有两端都成功才发布包含全部附件的 Release。
上传过程先创建草稿，附件上传成功后才公开。CI 不要求桌面会话或真实 GPU 播放验收。
不同版本的发布运行独立构建，不取消先前运行；Latest 按 GitHub 的版本及创建时间规则选择。

推送到 `dev` 不会自动触发构建。开发分支验证需在 GitHub Actions 页面选择
`Build and release`，点击 `Run workflow`，选择 `dev` 分支后手动运行；构建会执行
同一套来源检查、Linux 和 Windows 测试、Clippy、打包及 artifact 上传。

`workflow_dispatch` 支持手动运行任意包含该 workflow 的分支。选择 `master` 时，
会执行标签冲突检查，并在两端构建成功后创建标签和发布 Release，与 `master` push 相同。
选择 `dev` 或其他分支时，只执行验证、打包和 artifact 上传，不检查标签是否已存在，
不要求增加版本号，也不创建标签或发布 Release。
Linux 上传前校验 tar.gz 与 AppImage 的 SHA256；artifact 保留 7 天。GitHub 的手动运行
入口需要默认分支已包含此 workflow。

Linux CI 每次按版本号选择 FFmpeg `n9` 系列最新稳定发布标签，解析其对应提交，
并通过 Docker 构建参数 `FFMPEG_REF` 固定本次构建源码。Buildx 使用 GitHub Actions 的
`type=gha` 缓存持久化原生构建层；FFmpeg 是独立层，源码更新不重建 Rust 工具链、
Vulkan-Headers 或 libplacebo。
已加载的构建镜像通过 `package-linux.sh --skip-image-build` 用于打包，避免再次构建镜像。
实际 FFmpeg 提交及构建脚本保存在包内 `build-info/`。

输出到 `dist/`，其中 `<version>` 由 `Cargo.toml` 中的应用版本自动生成：

```text
tiny-player-<version>-linux-x86_64.tar.gz
tiny-player-<version>-linux-x86_64.tar.gz.sha256
tiny-player-<version>-linux-x86_64.manifest.json
tiny-player-<version>-linux-x86_64.AppImage
tiny-player-<version>-linux-x86_64.AppImage.sha256
```

归档根目录为 `tiny-player/`，包含 `bin/`、`lib/`、`share/`、`licenses/`、
`build-info/`、`manifest.json`、`README.txt` 和 `install.sh`。

AppImage 的 AppDir 使用平铺的 `bin/`、`lib/`、`share/`，而非 `usr/` 布局。
根目录 `AppRun -> bin/tiny-player`，desktop 和 icon 也使用指向 `share/` 内资源的
相对符号链接；移动或解压后仍可解析资源。其应用、库、许可证和构建信息来自同一 payload。

`packaging/linux/appimage-tools.json` 使用 appimagetool 与静态 musl type2-runtime 的
`continuous` release，固定当前快照的源提交和 SHA256。`continuous` 会滚动更新；采用
上游新产物时需同步更新锁文件中的摘要、源提交及对应许可/构建配方来源，再重建镜像。
Docker 缓存不会自动刷新工具，也不会自动接受后续产物；下载摘要不匹配时直接失败。
Docker 中准备并校验工具，打包显式传入 `--runtime-file`，不依赖 appimagetool 自动下载
runtime，也不需要容器 FUSE 或 privileged 模式。
清单的 `appimage_tools` 字段与包内 `build-info/appimage-tools.json` 记录工具锁定信息。
静态 runtime 不降低应用的 glibc 2.39 基线。工具准备在 root 下执行后仍允许普通
用户读取和执行；`--skip-image-build` 会先核对已缓存镜像中的工具及锁定信息，不匹配时
需重新构建镜像。

随包 `licenses/appimage-runtime/` 保留 runtime 许可证、libfuse LGPL 文本和静态依赖的
许可参考文本。锁文件记录 runtime 源码与构建配方；上游没有固定部分 Alpine 依赖的
实际构建版本，`notice_reference` 只表示固定许可文本，不表示已确认二进制对应源码版本。
这些记录不替代公开再分发所需的源码提供及 LGPL 静态链接义务核查。

## 依赖及兼容性检查

FFmpeg 保留软件解码器、解封装器、协议、音频滤镜、Vulkan 和 VA-API 硬解支持；显式启用
`--enable-vaapi --enable-libdrm`，构建环境安装 `libva-dev` 和 `libdrm-dev`。关闭编码器、
封装器、设备采集、命令行工具和无关外部库的自动探测。外部 AV1 解码使用 dav1d，
HTTPS 支持使用 GnuTLS。libplacebo 保留 Vulkan、LittleCMS 和 Dolby Vision reshaping，
Dolby Vision 元数据解析继续由项目的 Rust `dolby_vision` 依赖完成。

`bundle.py` 收集链接依赖，为主程序设置 `$ORIGIN/../lib` RUNPATH，为每个随包库设置
`$ORIGIN` RUNPATH。它检查所有 ELF 的 x86_64 架构、glibc 符号版本、完整依赖和库搜索
路径，并清除构建环境的库路径验证包内依赖的解析结果。
AppImage 封装后使用固定 runtime 的 `--appimage-extract` 解包，不执行播放器，
再次执行相同 ELF 审计并比较清单。出现 glibc > 2.39、缺库、意外从构建环境加载库
或解包后 ELF 不一致时，不输出本次发布产物。tar.gz 独有 `install.sh` 与 `README.txt`，
AppImage 不携带或调用目录安装脚本。

五个产物全部准备完成后，先复制到 `dist/` 同一文件系统的临时目录，再替换正式文件；
复制或替换失败时恢复原有产物，避免同版本的新旧归档、校验文件和清单混用。
恢复失败时保留 `.tiny-player-publish-*` 恢复目录并报告路径。不要向同一 `dist/` 并发打包；
这不是跨五个文件的原子事务，也不保证强制杀进程或掉电时自动恢复。

以下由目标系统提供，以匹配系统配置、插件和驱动：

- glibc 及 ELF 动态加载器。
- Vulkan loader 和适合显卡的 Vulkan 驱动。
- Ubuntu 24.04 或更新版本兼容的 `libva.so.2`、`libva-drm.so.2` 和 `libdrm.so.2`；
  这些库与动态加载的 VA-API 驱动都由宿主提供，避免包内旧版 loader 覆盖宿主驱动。
  使用 VA-API 时还需要对应驱动和 DRM render node 的访问权限。
- ALSA、声音设备配置和音频插件。
- Fontconfig、FreeType、字体及 XKB 键盘数据。
- Wayland 或 X11 会话；Wayland 客户端库。
- HTTPS 所需的系统 CA 证书。
- 播放抑制所需的 `xdg-desktop-portal` 和实现 Inhibit 接口的桌面后端；请求失败不阻止播放。
- Ubuntu 24.04 的 GCC 13 或更新版本提供的 GCC/C++ 运行库。使用宿主的
  `libstdc++.so.6` 和 `libgcc_s.so.1`，避免包内较旧运行库覆盖新显卡驱动所需的版本。

这不是 musl/Alpine 或 ARM 包。满足 glibc 版本仍不等于具备 Vulkan 硬解能力。

## 开发检查

在开发环境中按需运行，不属于打包步骤：

```sh
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --locked --all-targets -- -D warnings
python3 -B -m unittest discover -s packaging/linux -p 'test_*.py' -v
```

Linux 构建镜像内的测试还用小型 C 程序和共享库执行真实 AppImage 封装、解包审计、
中文/空格路径移动及无 FUSE 启动，检查相对 RUNPATH、资源与参数传递；不启动 Tiny Player。
宿主环境没有 `/opt/appimage` 时此 smoke test 会跳过，可用 `TINY_TEST_APPIMAGE_TOOLS`
指定已准备的工具目录。完整播放器编译、真实 FUSE 挂载和桌面播放仍需分别验证。

## 运行与安装

### AppImage

请将 `<version>` 替换为实际版本号，在下载目录校验并运行：

```sh
sha256sum --check "tiny-player-<version>-linux-x86_64.AppImage.sha256"
chmod +x "tiny-player-<version>-linux-x86_64.AppImage"
./tiny-player-<version>-linux-x86_64.AppImage
```

静态 musl runtime 无需 `libfuse2` 不等于无需 FUSE：正常运行仍通过 FUSE 挂载。
无 FUSE 或挂载受限时：

```sh
./tiny-player-<version>-linux-x86_64.AppImage --appimage-extract-and-run
```

也可在干净目录手动 `--appimage-extract`，然后运行 `./squashfs-root/AppRun`。
AppImage 不保证双击启动、自动菜单安装或自动更新。更新时关闭应用后替换文件。
与 tar.gz 共用原有用户配置和缓存路径；不改变宿主 Vulkan、音频、字体或 portal 边界。

### tar.gz

请将 `<version>` 替换为所下载包的实际版本号：

```sh
tar -xzf "tiny-player-<version>-linux-x86_64.tar.gz"
./tiny-player/bin/tiny-player
./tiny-player/install.sh
```

安装位置固定为 `~/.local/tiny-player.app`，脚本创建 `~/.local/bin/tiny-player` 链接和
桌面菜单项，无需 sudo，也无需传入参数。`--help` 可查看用法。

更新时关闭应用，将新版解压到新目录，再直接运行其中的 `install.sh`。
脚本先完整暂存新版，再替换应用目录、启动链接、桌面入口和
图标，清除旧版应用目录中不再随包提供的文件；安装失败时恢复原安装。应用配置和缓存
仍使用原来的用户目录。脚本拒绝覆盖无法识别的安装目录或已有的独立启动程序。

图标从实际可执行文件附近的 `share/tiny-player/assets` 加载，移动整个目录后仍可
运行。Arch 等系统包仍优先使用编译时的 `TINY_ASSET_DIR`；开发构建回退到源码资产。

## 手工桌面验收清单

以下由用户在真实桌面完成；CI 和打包审计不启动真实 GUI，也不替代播放验收。

- 校验两个 `.sha256` 文件；检查外部 `manifest.json` 的 `appimage_tools` 与包内
  `build-info/appimage-tools.json` 一致，确认工具 SHA256、ELF 依赖与 glibc 基线。
- 分别测试正常 FUSE 启动、`--appimage-extract-and-run`，以及手动解压后的 `AppRun`。
- 将 AppImage 或完整解压目录移动到含中文和空格的路径，确认启动、图标及资源加载正常。
- 在 Wayland 与 X11 会话分别检查窗口、全屏、键盘和鼠标交互。
- 测试软件解码、Vulkan 硬解，以及显式 VA-API 回传和 GPU 映射的 H.264、HEVC 10 位、暂停、seek 和 EOF；各路径仍需 Vulkan 渲染。GPU 映射还要求外部内存、DRM modifier 和同步导入能力，测试命令见播放引擎文档。
- 在支持 `VK_KHR_internally_synchronized_queues` 的 RADV 环境验证非零队列创建 flags，
  分别检查 tar.gz、安装后目录及 AppImage，
  确认加载捆绑的新 libplacebo；运行播放引擎文档中的显式队列回归测试。
  此项需要真实 GPU，不由普通 CI 或无 GPU 测试替代。
- 验证音频设备/音轨、字幕/字体、HDR 色调映射，以及 portal 播放抑制和释放。
- 测试手柄设备权限、快捷操作、失焦与断连行为。
- 关闭应用后替换新版本，在 AppImage 与 tar.gz 之间切换，确认跨版本配置和缓存仍共用原路径。
  记录系统、会话、GPU/驱动和测试媒体；不将一次机器验收视为所有发行版兼容保证。
