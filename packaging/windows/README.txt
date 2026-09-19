Tiny Player - Windows x64 portable package

Extract the entire ZIP, then double-click tiny-player.exe.
Move or copy the whole directory together, including DLLs and share/.
No installer, administrator privileges, Rust, Python, FFmpeg installation,
Visual Studio, or PATH changes are required on the destination machine.

Target: Windows 10 22H2 / Windows 11 x64 with current graphics drivers. The UI uses Direct3D;
video processing uses Vulkan. The Vulkan loader is included, but a working
Vulkan driver must be provided by your GPU vendor. Vulkan hardware decoding
additionally depends on the GPU/driver and codec. Software decode is the default.

Settings and playback caches are stored in your Windows user profile, not in
this directory. Moving the program does not move these settings.
Logs: %LOCALAPPDATA%\tiny-player\logs\tiny-player.log
Set TINY_LOG_FILE to override the log location (empty disables file logging).

This package is unsigned. It contains dynamically linked third-party libraries.
Licenses and source/build references are in licenses/ and build-info/.
manifest.json lists every payload file, its SHA256, and the audited DLL imports.

Project: https://github.com/Clwury/tiny-player
