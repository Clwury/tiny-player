"""Collect and verify the complete x64 portable application's DLL closure."""

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import shutil
import subprocess
import tomllib
import zipfile

from prepare import CACHE, LOCK_PATH, ROOT, load_tools, sha256

# Only OS components may be left out. Do not treat an arbitrary DLL found on
# this build machine as a Windows component (e.g. vcruntime or codec SDK DLLs).
SYSTEM_DLLS = set("""
advapi32.dll avrt.dll bcrypt.dll bcryptprimitives.dll cabinet.dll cfgmgr32.dll
combase.dll comctl32.dll comdlg32.dll crypt32.dll cryptbase.dll cryptnet.dll cryptsp.dll
d2d1.dll d3d11.dll d3d12.dll d3dcompiler_47.dll dcomp.dll dbghelp.dll dnsapi.dll
dwmapi.dll dwrite.dll dxgi.dll dxva2.dll gdi32.dll hid.dll icuuc.dll imm32.dll iphlpapi.dll
kernel32.dll kernelbase.dll mf.dll mfplat.dll mfreadwrite.dll mfuuid.dll
mmdevapi.dll mpr.dll msacm32.dll msimg32.dll msvcrt.dll ncrypt.dll netapi32.dll normaliz.dll
ntdll.dll ole32.dll oleacc.dll oleaut32.dll opengl32.dll powrprof.dll propsys.dll
psapi.dll rpcrt4.dll secur32.dll setupapi.dll shcore.dll shell32.dll shlwapi.dll
ucrtbase.dll uiautomationcore.dll user32.dll userenv.dll usp10.dll uxtheme.dll version.dll
windowscodecs.dll winhttp.dll wininet.dll winmm.dll winspool.drv wintrust.dll
wldap32.dll ws2_32.dll wtsapi32.dll
""".split())


def is_system(name):
    return name in SYSTEM_DLLS or name.startswith(("api-ms-win-", "ext-ms-win-"))


def inspect(path):
    import pefile

    with pefile.PE(str(path)) as pe:
        if pe.FILE_HEADER.Machine != 0x8664:
            raise RuntimeError(f"Not an x64 binary: {path}")
        dependencies = set()
        for directory in ("DIRECTORY_ENTRY_IMPORT", "DIRECTORY_ENTRY_DELAY_IMPORT"):
            dependencies.update(entry.dll.decode("ascii").lower()
                                for entry in getattr(pe, directory, []))
        for symbol in getattr(getattr(pe, "DIRECTORY_ENTRY_EXPORT", None), "symbols", []):
            if symbol.forwarder:
                name = symbol.forwarder.decode("ascii").split(".")[0].lower()
                dependencies.add(name + ".dll")
        return dict(machine="x86_64", subsystem=pe.OPTIONAL_HEADER.Subsystem,
                    imports=sorted(dependencies))


def verify(directory):
    binaries = {p.name.lower(): p for p in directory.iterdir()
                if p.suffix.lower() in (".dll", ".exe")}
    if "tiny-player.exe" not in binaries:
        raise RuntimeError("Missing tiny-player.exe")
    if "vulkan-1.dll" not in binaries:
        raise RuntimeError("Missing dynamically loaded Vulkan loader")
    inspected = {}
    for name, path in sorted(binaries.items()):
        metadata = inspect(path)
        for dependency in metadata["imports"]:
            if not is_system(dependency) and dependency not in binaries:
                raise RuntimeError(f"Missing {dependency}, required by {name}")
        inspected[name] = metadata
    if inspected["tiny-player.exe"]["subsystem"] != 2:
        raise RuntimeError("Expected a Windows GUI executable (no console window).")
    if not (directory / "share/tiny-player/assets/icons/play.svg").is_file():
        raise RuntimeError("Missing application assets")
    manifest_path = directory / "manifest.json"
    if manifest_path.is_file():
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        for name, expected in manifest["files"].items():
            path = directory / name
            if not path.is_file() or path.stat().st_size != expected["size"] or sha256(path) != expected["sha256"]:
                raise RuntimeError(f"Payload checksum mismatch: {name}")
    return inspected


def collect_rust_licenses(destination):
    metadata = json.loads(subprocess.check_output([
        "cargo", "metadata", "--locked", "--offline", "--format-version", "1",
        "--filter-platform", "x86_64-pc-windows-msvc",
    ], cwd=ROOT, text=True, encoding="utf-8"))
    resolved = {node["id"] for node in metadata["resolve"]["nodes"]}
    index = []
    for package in metadata["packages"]:
        if package["id"] not in resolved:
            continue
        source = Path(package["manifest_path"]).parent
        files = set()
        if package.get("license_file"):
            files.add(source / package["license_file"])
        # Workspace crates may inherit their license file from the repo root.
        for parent in [source, *list(source.parents)[:3]]:
            for pattern in ("LICENSE*", "LICENCE*", "COPYING*", "NOTICE*"):
                files.update(path for path in parent.glob(pattern) if path.is_file())
            if files:
                break
        folder = destination / f"{package['name']}-{package['version']}"
        folder.mkdir(parents=True, exist_ok=True)
        for path in files:
            shutil.copy2(path, folder / path.name)
        index.append({key: package.get(key) for key in
                      ("name", "version", "license", "repository", "source")})
    (destination / "index.json").write_text(json.dumps(index, indent=2) + "\n")


def build_bundle():
    lock = json.loads(LOCK_PATH.read_text())
    load_tools(lock, offline=True)
    paths = json.loads((CACHE / "paths.json").read_text())
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    dist = ROOT / "dist"
    name = f"tiny-player-{version}-windows-x86_64"
    destination = dist / name
    stage = CACHE / ("bundle-" + datetime.now().strftime("%Y%m%d-%H%M%S-%f"))
    stage.mkdir(parents=True)
    executable = ROOT / "target/x86_64-pc-windows-msvc/release/tiny-player.exe"
    shutil.copy2(executable, stage / "tiny-player.exe")
    sources = {}
    for directory in (Path(paths["ffmpeg"]) / "bin", Path(paths["msys"]) / "bin"):
        for dll in directory.glob("*.dll"):
            key = dll.name.lower()
            if key in sources and sha256(sources[key]) != sha256(dll):
                raise RuntimeError(f"Conflicting DLL: {key}")
            sources[key] = dll
    pending = ["tiny-player.exe"]
    copied = set()
    # Vulkan can also be loaded dynamically. Ship the pinned Khronos loader,
    # while the GPU's actual Vulkan driver remains supplied by the system.
    pending.append("vulkan-1.dll")
    while pending:
        dependency = pending.pop()
        if dependency in copied or is_system(dependency):
            continue
        target = stage / dependency
        if not target.exists():
            source = sources.get(dependency)
            if source is None:
                raise RuntimeError(f"No pinned provider for {dependency}")
            shutil.copy2(source, target)
        copied.add(dependency)
        pending.extend(inspect(target)["imports"])
    shutil.copytree(ROOT / "assets", stage / "share/tiny-player/assets")
    shutil.copy2(Path(__file__).with_name("README.txt"), stage / "README.txt")
    shutil.copy2(ROOT / "LICENSE", stage / "LICENSE")
    licenses = stage / "licenses"
    shutil.copytree(Path(paths["msys"]) / "share/licenses", licenses / "native")
    collect_rust_licenses(licenses / "rust")
    (licenses / "ffmpeg").mkdir(parents=True)
    shutil.copy2(Path(paths["ffmpeg"]) / "LICENSE.txt", licenses / "ffmpeg/LICENSE.txt")
    # Preserve the distributor's detailed notices/build information as supplied.
    if (Path(paths["ffmpeg"]) / "doc").is_dir():
        shutil.copytree(Path(paths["ffmpeg"]) / "doc", licenses / "ffmpeg/doc")
    build_info = stage / "build-info"
    build_info.mkdir()
    shutil.copy2(LOCK_PATH, build_info / LOCK_PATH.name)
    shutil.copy2(ROOT / "Cargo.lock", build_info / "Cargo.lock")
    shutil.copytree(CACHE / "native/package-info", build_info / "native-packages")
    (licenses / "SOURCES.txt").write_text(
        "Native library source and build recipes:\n"
        + lock["ffmpeg"]["build_source"] + "\n"
        + "https://github.com/FFmpeg/FFmpeg/tree/946fcce07b\n"
        + "https://github.com/msys2/MINGW-packages\n"
        + "MSYS2 package build metadata and recipe SHA256: ../build-info/native-packages/*/BUILDINFO\n"
        + "Exact binary versions, URLs and hashes: ../build-info/native-lock.json\n"
        + "FFmpeg and libplacebo are dynamically linked and can be replaced with ABI-compatible DLLs.\n",
        encoding="utf-8")
    binaries = verify(stage)
    manifest = dict(
        name="Tiny Player", version=version, target=lock["target"],
        built_at=datetime.now(timezone.utc).isoformat(),
        rustc=subprocess.check_output(["rustc", "-V"], text=True).strip(),
        commit=subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        source_modified=bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT)),
        native_lock_sha256=sha256(LOCK_PATH), binaries=binaries,
        files={str(p.relative_to(stage)).replace("\\", "/"): dict(size=p.stat().st_size, sha256=sha256(p))
               for p in sorted(stage.rglob("*")) if p.is_file()},
    )
    (stage / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    dist.mkdir(exist_ok=True)
    # Keep an earlier result rather than deleting arbitrary directories.
    if destination.exists():
        destination.rename(dist / (name + ".previous-" + datetime.now().strftime("%Y%m%d-%H%M%S-%f")))
    stage.rename(destination)
    archive = dist / (name + ".zip")
    partial_archive = archive.with_suffix(".zip.partial")
    # Cargo's unpacked license files can carry Unix epoch timestamps.
    with zipfile.ZipFile(partial_archive, "w", compression=zipfile.ZIP_DEFLATED,
                         compresslevel=6, strict_timestamps=False) as output:
        for path in sorted(destination.rglob("*")):
            if path.is_file():
                output.write(path, str(Path(name) / path.relative_to(destination)))
    partial_archive.replace(archive)
    archive.with_suffix(".zip.sha256").write_text(f"{sha256(archive)}  {archive.name}\n")
    print(f"Verified {len(binaries)} x64 binaries: {destination}")
    print(f"Portable ZIP: {archive} ({archive.stat().st_size / 1024**2:.1f} MiB)")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verify", type=Path)
    args = parser.parse_args()
    if args.verify:
        load_tools(json.loads(LOCK_PATH.read_text()), offline=True)
        print(json.dumps(verify(args.verify), indent=2))
    else:
        build_bundle()
