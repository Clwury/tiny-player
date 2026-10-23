Tiny Player — Linux x86_64 prebuilt bundle

Requirements
  * x86_64 GNU/Linux, glibc 2.39 or newer (not Alpine/musl).
  * A working Vulkan loader (libvulkan.so.1) and a compatible GPU driver.
  * Host libva.so.2, libva-drm.so.2 and libdrm.so.2 compatible with Ubuntu 24.04
    or newer. VA-API decoding additionally needs a matching host driver and
    access to a DRM render node.
  * ALSA (libasound.so.2), including the host's audio configuration/plugins.
  * Fontconfig, FreeType, installed fonts and XKB keyboard data.
  * A Wayland or X11 desktop session; Wayland requires libwayland-client.so.0.
  * The system CA certificate store for HTTPS connections.
  * xdg-desktop-portal and an Inhibit-capable desktop backend for playback
    idle inhibition. A rejected request is logged and does not stop playback.
  * GCC/C++ runtime libraries compatible with Ubuntu 24.04 (GCC 13) or newer.

Run
  Extract the entire directory and run ./tiny-player/bin/tiny-player.
  Keep bin/, lib/ and share/ together. The directory may be moved.

AppImage (alternative to tar.gz)
  Replace <version> with the downloaded version; verify in the download directory:
  sha256sum --check "tiny-player-<version>-linux-x86_64.AppImage.sha256"
  chmod +x "tiny-player-<version>-linux-x86_64.AppImage"
  ./tiny-player-<version>-linux-x86_64.AppImage
  Without FUSE:
  ./tiny-player-<version>-linux-x86_64.AppImage --appimage-extract-and-run
  Or use --appimage-extract in a fresh directory, then ./squashfs-root/AppRun.
  The static musl runtime needs no libfuse2, but normal mounting still needs FUSE.
  The application still requires glibc 2.39 and the host dependencies above.
  Embedded GitHub Release update information and the matching .zsync file support
  external AppImageUpdate tools. Close Tiny Player before updating or replacing
  the AppImage. Double-click launching and menu installation depend on the desktop.
  AppImage and tar.gz
  share the existing user settings/cache paths; no portable configuration is created.

Install tar.gz (optional, no sudo)
  cd tiny-player
  ./install.sh
  The application is installed at ~/.local/tiny-player.app and gets a desktop entry.
  The installation location is fixed; no directory argument is accepted.
  To update, close Tiny Player, extract the new bundle into a fresh directory,
  and run its install.sh without arguments. The installer replaces the old
  application and refreshes its launcher, desktop entry and icons. Obsolete
  Tiny Player icon sizes are removed, and KDE sessions are notified to reload icons.
  If installation fails, it restores the previous version, including its icons.
  User settings and caches are preserved.

The bundle carries FFmpeg, libplacebo and their selected library dependencies.
glibc, GPU drivers, audio plugins and font/keyboard configuration come from the
host. Hardware decoding still depends on the GPU and driver capabilities.
Software decoding also needs Vulkan for rendering. AppImage uses the same audited
payload and host-library exclusions as tar.gz; it does not bundle GPU drivers.

VA-API copyback (Linux)
  Select Force VA-API (copyback) in playback settings, or set TINY_HWDEC=vaapi.
  Optionally set TINY_VAAPI_DEVICE=/dev/dri/renderD128 to select a device.
  Frames retain their original bit depth and pass through Vulkan/libplacebo.
  This forced mode reports failures without switching decoders; Auto still
  prefers Vulkan. VA-API/libdrm loaders and drivers come from the host.

VA-API to Vulkan GPU mapping (Linux)
  Select Force VA-API (map to Vulkan), or set TINY_HWDEC=vaapi-vulkan.
  Uses the same TINY_VAAPI_DEVICE setting. NV12/P010 frames are imported through
  DMA-BUF, requiring Vulkan external memory, DRM modifiers and synchronization.
  Mapping/render failures are reported without switching to CPU copyback.
  Stats show HW: vaapi-vulkan. Final BGRA output still returns to GPUI memory.

Packaging
  package-linux.sh outputs tar.gz, AppImage, AppImage.zsync, both SHA256 files and manifest.json
  by default. AppDir has flat bin/, lib/, share/ directories; AppRun points to
  bin/tiny-player and root desktop/icon links use relative targets in share/.
  packaging/linux/appimage-tools.json pins appimagetool and static musl type2-runtime
  with SHA256. Docker prepares verified tools; packaging uses --runtime-file,
  without container FUSE or privileged mode.

Manual desktop checks (performed by the user, not headless CI)
  Verify checksums and tool metadata; test normal and no-FUSE startup, relocation
  into paths with Chinese characters/spaces, Wayland/X11, software/Vulkan/VA-API
  decoding, audio, subtitles/fonts, HDR, portal inhibition/release and gamepads.
  Close the app before replacing versions; verify shared settings/cache paths
  across versions and between AppImage and tar.gz.

manifest.json lists the bundled ELF files, dependencies, checksums and required
glibc versions. Its appimage_tools field records pinned tool metadata.
build-info/ records native source revisions, FFmpeg options and appimage-tools.json.
Licenses and notices are in licenses/. Packaging sources and instructions:
https://github.com/Clwury/tiny-player/tree/master/packaging/linux
https://github.com/Clwury/tiny-player/blob/master/docs/linux-packaging.md
