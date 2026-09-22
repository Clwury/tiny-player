# Tiny Player

English | [简体中文](README.zh.md)

Tiny Player is a native Emby desktop client for Linux and Windows, built with Rust and GPUI. It uses FFmpeg for decoding, with Vulkan and libplacebo handling video rendering and color processing.

## Features

- **Multiple servers**: Add, edit, and switch between Emby servers, with server icons and media counts.
- **Library browsing**: Browse movie and TV posters, search, manage favorites, continue watching, and select seasons and episodes.
- **Media details**: View title logos, overviews, ratings, runtime, release dates, resolution, and dynamic range information.
- **Playback and track selection**: Choose video versions, audio tracks, and subtitles, set language preferences, and sync playback progress.
- **Video processing**: Software decoding, Vulkan hardware decoding, and HDR tone mapping. Hardware decoding support depends on the GPU, driver, and video codec.
- **Playback controls**: Fullscreen, playback speed adjustment, subtitle positioning, a playback information overlay, and configurable memory and disk caches.

## Screenshots

![Tiny Player media details and episode selection](docs/tiny-player-1.png)

![Tiny Player video playback and controls](docs/tiny-player-2.png)

## System Requirements

| Platform | Requirements |
| --- | --- |
| Linux prebuilt bundle | x86_64 GNU/Linux, glibc 2.39 or later, and a Wayland or X11 desktop session |
| Windows portable bundle | Windows 10 22H2 / Windows 11 x64, with a GPU and driver supporting Direct3D 11 |
| Video processing | A working Vulkan GPU driver, including when software decoding is used |

The Linux prebuilt bundle includes FFmpeg, libplacebo, and selected dependencies. The system must also provide a Vulkan loader, ALSA and audio configuration, Fontconfig, FreeType, fonts, XKB keyboard data, system CA certificates, and GCC/C++ runtime libraries compatible with Ubuntu 24.04 (GCC 13) or later. The glibc 2.39 baseline applies to the prebuilt bundle; requirements for other builds depend on their build environment and dependencies.

The Windows portable bundle includes multimedia dependencies and a Vulkan loader. The system must provide the GPU's Vulkan driver. Running the portable bundle does not require Rust, Python, Visual Studio, or a separate FFmpeg installation.

## Installation

### Arch Linux

Build and install using the repository's [PKGBUILD](PKGBUILD):

```sh
sudo pacman -S --needed base-devel git
git clone https://github.com/Clwury/tiny-player.git
cd tiny-player
makepkg -si
```

`makepkg` installs the declared build and runtime dependencies. Once installed, launch Tiny Player from your desktop application menu or run:

```sh
tiny-player
```

### Linux Prebuilt Bundle

Extract the complete archive and run the application. The following example uses version `0.1.0`; replace the version number as needed:

```sh
tar -xzf tiny-player-0.1.0-linux-x86_64.tar.gz
./tiny-player/bin/tiny-player
```

You can also install it for the current user without `sudo`:

```sh
./tiny-player/install.sh
```

The installer uses the fixed location `~/.local/tiny-player.app`, creates a launcher symlink at `~/.local/bin/tiny-player`, and adds a desktop menu entry. To update, close Tiny Player, extract the new bundle into a fresh directory, and run its installer without arguments. It replaces the application, refreshes the launcher, desktop entry and icons, and restores the previous installation if an update fails. User settings and caches are preserved.

To build the prebuilt bundle yourself, set up Docker on an x86_64 Linux host and run the following from the repository root:

```sh
./scripts/package-linux.sh
```

The archive, checksum file, and build manifest are written to `dist/`. See the [Linux packaging guide](docs/linux-packaging.md) for details.

### Windows Portable Bundle

Extract the complete `tiny-player-<version>-windows-x86_64.zip` archive and double-click `tiny-player.exe`. Keep the entire directory together when moving the application, including its DLLs and `share/` resources. Settings and caches are stored in your Windows user profile.

Building the bundle requires the Rust MSVC toolchain, Visual Studio 2022 C++ x64 Build Tools, the Windows SDK, and 64-bit Python 3.12. Run the following in PowerShell from the repository root:

```powershell
.\scripts\package-windows.ps1
```

The script prepares native dependencies and generates the application directory and ZIP in `dist/`. See the [Windows packaging guide](docs/windows-packaging.md) for environment setup, offline builds, and validation commands.

### Running from Source

After cloning the repository, prepare the following dependencies for Linux development:

- Stable Rust, C/C++ build tools, Clang/libclang, and pkg-config.
- FFmpeg 8.1 or 9.x, libplacebo 7.x, and their development headers.
- Vulkan headers, a loader, and a GPU driver, along with ALSA, Wayland/X11, and other dependencies listed in [PKGBUILD](PKGBUILD).

Run from the repository root:

```sh
cargo run --locked
```

To build an optimized version:

```sh
cargo build --release --locked
./target/release/tiny-player
```

For Windows development, meet the Windows build requirements above, then prepare the dependencies and local Cargo configuration:

```powershell
.\scripts\package-windows.ps1 -Mode Prepare
cargo run --locked
```

## Playback Controls and Keyboard Shortcuts

These shortcuts are active when the playback page has keyboard focus:

| Key | Action |
| --- | --- |
| `Space` / `P` | Play / pause |
| `F` | Toggle fullscreen |
| `Esc` | Close the episode list if open; otherwise, exit fullscreen |
| `←` / `→` | Seek backward / forward 5 seconds |
| `↓` / `↑` | Seek backward / forward 60 seconds |
| `9` / `/` | Decrease volume by 2 percentage points |
| `0` / `*` | Increase volume by 2 percentage points |
| `M` | Mute / unmute |
| `[` | Divide playback speed by 1.1 |
| `]` | Multiply playback speed by 1.1 |
| `{` / `}` | Halve / double playback speed (usually `Shift` + `[` / `]`) |
| `Backspace` | Reset playback speed to 1× |
| `R` / `T` | Move subtitles up / down by 1% of the displayed video height |
| `I` | Show / hide playback information |

Playback speed ranges from **0.25× to 4×**. Volume shortcuts also support the numeric keypad's divide and multiply keys. Holding a volume or speed adjustment key repeats the adjustment. Arrow keys trigger one seek per key press.

Mouse controls over the playback area:

| Action | Effect |
| --- | --- |
| Double-click the video with the left mouse button | Toggle fullscreen |
| Right-click the video | Play / pause |
| Scroll up / down | Increase / decrease volume |
| Click or drag the progress bar | Seek to the selected position |

## License

This project is licensed under the **GNU General Public License v3.0 only (GPL-3.0-only)**. See [LICENSE](LICENSE) for the full terms. Third-party dependencies retain their respective licenses.

## Acknowledgments

Thanks to the following projects and applications for their foundations, implementation references, and design inspiration:

- [Zed / GPUI](https://github.com/zed-industries/zed)
- [gpui-kit](https://github.com/longbridge/gpui-kit)
- [Tsukimi](https://github.com/tsukinaha/tsukimi)
- [mpv](https://github.com/mpv-player/mpv)
- [Lenna](https://lennaapp.github.io/)

icons come from [lige47/lige_icon (离歌图标库)](https://github.com/lige47/lige_icon)
