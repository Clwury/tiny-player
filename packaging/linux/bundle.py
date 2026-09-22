#!/usr/bin/env python3
"""Assemble and audit the Linux prebuilt bundle; use only inside the builder."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib


BASELINE = (2, 39)
# These libraries must match the host's libc, drivers, fonts and audio plugins.
HOST_LIBRARIES = {
    "ld-linux-x86-64.so.2", "libc.so.6", "libm.so.6", "libmvec.so.1",
    "libdl.so.2", "libpthread.so.0", "librt.so.1", "libresolv.so.2",
    "libutil.so.1", "libanl.so.1", "libasound.so.2", "libvulkan.so.1",
    "libfontconfig.so.1", "libfreetype.so.6", "libwayland-client.so.0",
    "libwayland-cursor.so.0", "libwayland-egl.so.1",
    "libstdc++.so.6", "libgcc_s.so.1",
}
RUNTIME_HOST_LIBRARIES = [
    "libasound.so.2", "libvulkan.so.1", "libfontconfig.so.1",
    "libfreetype.so.6", "libwayland-client.so.0",
    "libstdc++.so.6", "libgcc_s.so.1",
]


def run(*args, clean_loader=False):
    env = dict(os.environ, LC_ALL="C")
    if clean_loader:
        for name in ("LD_LIBRARY_PATH", "LD_PRELOAD", "LD_AUDIT"):
            env.pop(name, None)
    return subprocess.check_output(args, text=True, env=env, stderr=subprocess.STDOUT)


def required_glibc(version_info):
    # Defined/exported versions are not requirements. Audit .gnu.version_r only.
    needs = version_info.partition("Version needs section")[2]
    names = set(re.findall(r"Name: (GLIBC_[A-Za-z0-9_.]+)", needs))
    versions = []
    for name in names:
        if name == "GLIBC_ABI_DT_RELR":  # Supported since glibc 2.36.
            versions.append((2, 36))
        elif re.fullmatch(r"GLIBC_[0-9]+(?:\.[0-9]+)+", name):
            versions.append(tuple(map(int, name.removeprefix("GLIBC_").split("."))))
        else:
            raise ValueError(f"Unsupported glibc requirement: {name}")
    return max(versions, default=(0, 0))


def inspect_elf(path):
    header = run("readelf", "--file-header", "--wide", str(path))
    if not (re.search(r"Class:\s+ELF64", header)
            and re.search(r"Machine:\s+Advanced Micro Devices X86-64", header)):
        raise ValueError(f"Not an x86_64 ELF64 file: {path}")
    version_info = run("readelf", "--version-info", "--wide", str(path))
    minimum = required_glibc(version_info)
    if minimum > BASELINE:
        raise ValueError(f"{path.name} requires glibc {version_text(minimum)}, exceeds 2.39")
    dynamic = run("readelf", "--dynamic", "--wide", str(path))
    return {
        "glibc_required": version_text(minimum),
        "glibcxx_required": sorted(set(re.findall(
            r"Name: (GLIBCXX_[0-9.]+)", version_info.partition("Version needs section")[2]
        ))),
        "needed": re.findall(r"\(NEEDED\).*\[(.*?)\]", dynamic),
        "runpath": re.findall(r"\((?:RPATH|RUNPATH)\).*\[(.*?)\]", dynamic),
    }


def version_text(version):
    return ".".join(map(str, version))


def parse_ldd(output):
    resolved = {}
    for line in output.splitlines():
        if "not found" in line or "version `" in line:
            raise ValueError(f"Unresolved dependency: {line.strip()}")
        match = re.match(r"\s*(\S+) => (/.+) \(0x[0-9a-f]+\)\s*$", line)
        if match:
            resolved[Path(match[1]).name] = Path(match[2])
        elif "=>" in line:
            raise ValueError(f"Unrecognized dependency: {line.strip()}")
    if not resolved:
        raise ValueError("ldd did not report any resolved shared libraries")
    return resolved


def copy_package_copyright(library, licenses):
    # Debian's merged-/usr can record ownership under either spelling.
    candidates = [str(library), str(library.resolve())]
    candidates += [p.removeprefix("/usr") for p in candidates if p.startswith("/usr/lib/")]
    for candidate in candidates:
        result = subprocess.run(["dpkg-query", "-S", candidate], text=True, capture_output=True)
        if result.returncode:
            continue
        package = result.stdout.split(": ", 1)[0].split(":", 1)[0]
        copyright_file = Path("/usr/share/doc") / package / "copyright"
        if copyright_file.exists():
            target = licenses / package
            target.mkdir(exist_ok=True)
            shutil.copy2(copyright_file, target / "copyright")
            return
    if not str(library).startswith("/opt/tiny-player/"):
        raise ValueError(f"Could not find copyright metadata for {library}")


def audit_bundle(root):
    files = [root / "bin/tiny-player", *sorted((root / "lib").iterdir())]
    manifest = {}
    for path in files:
        info = inspect_elf(path)
        expected_rpath = "$ORIGIN/../lib" if path.parent.name == "bin" else "$ORIGIN"
        if info["runpath"] != [expected_rpath]:
            raise ValueError(f"Unexpected library search path in {path}: {info['runpath']}")
        for needed in info["needed"]:
            if "/" in needed:
                raise ValueError(f"Non-relocatable dependency: {path.name} -> {needed}")
            if needed not in HOST_LIBRARIES and not (root / "lib" / needed).is_file():
                raise ValueError(f"Missing bundled dependency: {path.name} -> {needed}")
        info["sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
        manifest[str(path.relative_to(root))] = info
    # Resolve the bundled application without the builder's LD_LIBRARY_PATH.
    for name, resolved in parse_ldd(run("ldd", str(files[0]), clean_loader=True)).items():
        if name not in HOST_LIBRARIES and not resolved.resolve().is_relative_to(root.resolve()):
            raise ValueError(f"Dependency escaped bundle: {name} -> {resolved}")
    return manifest


def build_bundle(binary, output):
    repo = Path(__file__).resolve().parents[2]
    if run("getconf", "GNU_LIBC_VERSION").strip() != "glibc 2.39":
        raise ValueError("Build inside the Ubuntu 24.04 / glibc 2.39 container")
    inspect_elf(binary)
    version = tomllib.loads((repo / "Cargo.toml").read_text())["package"]["version"]
    artifact_name = f"tiny-player-{version}-linux-x86_64"
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="tiny-player-bundle-") as staging:
        root = Path(staging) / "tiny-player"
        for folder in ("bin", "lib", "licenses", "share/applications"):
            (root / folder).mkdir(parents=True)
        shutil.copy2(binary, root / "bin/tiny-player")
        subprocess.run(["strip", "--strip-unneeded", str(root / "bin/tiny-player")], check=True)
        for name, source in sorted(parse_ldd(run("ldd", str(binary))).items()):
            if name in HOST_LIBRARIES:
                continue
            inspect_elf(source)
            target = root / "lib" / name
            shutil.copy2(source, target)
            subprocess.run(["patchelf", "--set-rpath", "$ORIGIN", str(target)], check=True)
            copy_package_copyright(source, root / "licenses")
        subprocess.run([
            "patchelf", "--set-rpath", "$ORIGIN/../lib", str(root / "bin/tiny-player")
        ], check=True)
        shutil.copytree(repo / "assets", root / "share/tiny-player/assets")
        shutil.copy2(repo / "tiny-player.desktop", root / "share/applications")
        for size, extension in (("512x512", "png"), ("scalable", "svg")):
            icons = root / "share/icons/hicolor" / size / "apps"
            icons.mkdir(parents=True)
            shutil.copy2(repo / f"assets/icons/tiny-player.{extension}", icons)
        shutil.copy2(repo / "LICENSE", root / "licenses/tiny-player-GPL-3.0-only.txt")
        shutil.copytree("/opt/tiny-player/share/licenses", root / "licenses/native", dirs_exist_ok=True)
        shutil.copytree("/opt/tiny-player/share/build-info", root / "build-info")
        shutil.copy2(repo / "Cargo.lock", root / "build-info/Cargo.lock")
        shutil.copy2(repo / "packaging/linux/build-native.sh", root / "build-info/build-native.sh")
        shutil.copy2(repo / "packaging/linux/install.sh", root / "install.sh")
        (root / "install.sh").chmod(0o755)
        shutil.copy2(repo / "packaging/linux/README.txt", root / "README.txt")
        subprocess.run(["desktop-file-validate", str(root / "share/applications/tiny-player.desktop")], check=True)
        manifest = {
            "version": version,
            "architecture": "x86_64",
            "glibc_baseline": "2.39",
            "runtime_host_libraries": RUNTIME_HOST_LIBRARIES,
            "elf_files": audit_bundle(root),
        }
        manifest_text = json.dumps(manifest, indent=2, sort_keys=True) + "\n"
        (root / "manifest.json").write_text(manifest_text)
        archive = output / f"{artifact_name}.tar.gz"
        temporary_archive = Path(staging) / archive.name
        with tarfile.open(temporary_archive, "w:gz") as tar:
            tar.add(root, arcname="tiny-player")
        shutil.move(temporary_archive, archive)
        (output / f"{artifact_name}.manifest.json").write_text(manifest_text)
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        (output / f"{artifact_name}.tar.gz.sha256").write_text(f"{digest}  {archive.name}\n")
        print(f"Created {archive} ({archive.stat().st_size / 1024**2:.1f} MiB)")
        print(f"Audited {len(manifest['elf_files'])} x86_64 ELF files: glibc <= 2.39")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    arguments = parser.parse_args()
    build_bundle(arguments.binary.resolve(), arguments.output.resolve())
