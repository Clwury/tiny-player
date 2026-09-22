Tiny Player — Linux x86_64 prebuilt bundle

Requirements
  * x86_64 GNU/Linux, glibc 2.39 or newer (not Alpine/musl).
  * A working Vulkan loader (libvulkan.so.1) and a compatible GPU driver.
  * ALSA (libasound.so.2), including the host's audio configuration/plugins.
  * Fontconfig, FreeType, installed fonts and XKB keyboard data.
  * A Wayland or X11 desktop session; Wayland requires libwayland-client.so.0.
  * The system CA certificate store for HTTPS connections.
  * GCC/C++ runtime libraries compatible with Ubuntu 24.04 (GCC 13) or newer.

Run
  Extract the entire directory and run ./tiny-player/bin/tiny-player.
  Keep bin/, lib/ and share/ together. The directory may be moved.

Install (optional, no sudo)
  cd tiny-player
  ./install.sh
  The application is installed at ~/.local/tiny-player.app and gets a desktop entry.
  The installation location is fixed; no directory argument is accepted.
  To update, close Tiny Player, extract the new bundle into a fresh directory,
  and run its install.sh without arguments. The installer replaces the old
  application and refreshes its launcher, desktop entry and icons. If installation
  fails, it restores the previous version. User settings and caches are preserved.

The bundle carries FFmpeg, libplacebo and their selected library dependencies.
glibc, GPU drivers, audio plugins and font/keyboard configuration come from the
host. Hardware decoding still depends on the GPU and driver capabilities.

manifest.json lists the bundled ELF files, dependencies, checksums and required
glibc versions. build-info/ records native source revisions and FFmpeg options.
Licenses and notices are in licenses/. Packaging sources and instructions:
https://github.com/Clwury/tiny-player/tree/main/packaging/linux
