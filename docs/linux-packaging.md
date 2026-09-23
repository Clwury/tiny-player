# Linux 预编译包

目标为 **x86_64 GNU/Linux、glibc 2.39 或更新版本**。整个程序及随包原生库在
Ubuntu 24.04 容器中编译；不使用宿主机的 FFmpeg、libplacebo 或 glibc。
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

打包入口只执行 release 构建、依赖收集、ELF 检查和归档；开发检查单独运行。
依赖版本由 `Cargo.lock` 和 `packaging/linux/build-native.sh` 固定：
FFmpeg 9.0.1、libplacebo 7.360.1、Vulkan-Headers 1.4.357；Rust 为 1.97.0。
Ubuntu 安全更新随构建时的仓库更新，镜像不承诺逐字节可复现。

输出到 `dist/`，其中 `<version>` 由 `Cargo.toml` 中的应用版本自动生成：

```text
tiny-player-<version>-linux-x86_64.tar.gz
tiny-player-<version>-linux-x86_64.tar.gz.sha256
tiny-player-<version>-linux-x86_64.manifest.json
```

归档根目录为 `tiny-player/`，包含 `bin/`、`lib/`、`share/`、`licenses/`、
`build-info/`、`manifest.json`、`README.txt` 和 `install.sh`。

## 依赖及兼容性检查

FFmpeg 保留软件解码器、解封装器、协议、音频滤镜和 Vulkan 硬解支持；关闭编码器、
封装器、设备采集、命令行工具和无关外部库的自动探测。外部 AV1 解码使用 dav1d，
HTTPS 支持使用 GnuTLS。libplacebo 保留 Vulkan、LittleCMS 和 Dolby Vision reshaping，
Dolby Vision 元数据解析继续由项目的 Rust `dolby_vision` 依赖完成。

`bundle.py` 收集链接依赖，为主程序设置 `$ORIGIN/../lib` RUNPATH，为每个随包库设置
`$ORIGIN` RUNPATH。它检查所有 ELF 的 x86_64 架构、glibc 符号版本、完整依赖和库搜索
路径，并清除构建环境的库路径验证包内依赖的解析结果。
出现 glibc > 2.39、缺库或意外从构建环境加载库时，不生成发布归档。

以下由目标系统提供，以匹配系统配置、插件和驱动：

- glibc 及 ELF 动态加载器。
- Vulkan loader 和适合显卡的 Vulkan 驱动。
- ALSA、声音设备配置和音频插件。
- Fontconfig、FreeType、字体及 XKB 键盘数据。
- Wayland 或 X11 会话；Wayland 客户端库。
- HTTPS 所需的系统 CA 证书。
- Ubuntu 24.04 的 GCC 13 或更新版本提供的 GCC/C++ 运行库。使用宿主的
  `libstdc++.so.6` 和 `libgcc_s.so.1`，避免包内较旧运行库覆盖新显卡驱动所需的版本。

这不是 musl/Alpine 或 ARM 包。满足 glibc 版本仍不等于具备 Vulkan 硬解能力。

## 开发检查

在开发环境中按需运行，不属于打包步骤：

```sh
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
python3 -B -m unittest discover -s packaging/linux -p 'test_*.py' -v
```

## 运行与安装

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
