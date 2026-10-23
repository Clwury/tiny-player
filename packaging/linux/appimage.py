#!/usr/bin/env python3
"""Prepare pinned AppImage tools and wrap an already audited Linux payload."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
from urllib.parse import quote, unquote, urlsplit


LOCK_FILE = Path(__file__).with_name("appimage-tools.json")
TOOLS_DIR = Path("/opt/appimage")
APPDIR_LINKS = {
    "AppRun": "bin/tiny-player",
    "tiny-player.desktop": "share/applications/tiny-player.desktop",
    "tiny-player.png": "share/icons/hicolor/256x256/apps/tiny-player.png",
    ".DirIcon": "tiny-player.png",
}


def read_lock(path=LOCK_FILE):
    lock = json.loads(path.read_text())
    if lock.get("architecture") != "x86_64":
        raise ValueError("AppImage tools must target x86_64")
    # Validate all pins before downloading or executing anything.
    for relative, artifact in tool_files(lock):
        if not re.fullmatch(r"[0-9a-f]{64}", artifact.get("sha256", "")):
            raise ValueError(f"Invalid SHA256 for {relative}")
        if not artifact.get("url", "").startswith("https://"):
            raise ValueError(f"Expected an HTTPS URL for {relative}")
    return lock


def tool_files(lock):
    yield "appimagetool-x86_64.AppImage", lock["appimagetool"]
    yield "runtime-x86_64", lock["runtime"]
    yield "licenses/runtime.txt", lock["runtime_license"]
    names = set()
    for license_info in lock.get("third_party_licenses", []):
        name = license_info["name"]
        if not re.fullmatch(r"[a-z0-9-]+", name) or name in names or name == "runtime":
            raise ValueError(f"Invalid or duplicate runtime license name: {name}")
        names.add(name)
        yield f"licenses/{name}.txt", license_info


def sha256(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def verify_file(path, artifact):
    if not path.is_file() or sha256(path) != artifact["sha256"]:
        raise ValueError(f"Missing or mismatched pinned AppImage file: {path}; rebuild the builder image")


def prepare_tools(destination, lock_file=LOCK_FILE):
    lock = read_lock(lock_file)
    destination.parent.mkdir(parents=True, exist_ok=True)
    # A failed download/checksum must never leave an executable in the tool directory.
    with tempfile.TemporaryDirectory(prefix="appimage-tools-", dir=destination.parent) as temporary:
        staging = Path(temporary)
        for relative, artifact in tool_files(lock):
            target = staging / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            subprocess.run([
                "curl", "--fail", "--location", "--retry", "3",
                "--proto", "=https", "--proto-redir", "=https", "--output", str(target), artifact["url"],
            ], check=True)
            verify_file(target, artifact)
        (staging / "appimagetool-x86_64.AppImage").chmod(0o755)
        (staging / "runtime-x86_64").chmod(0o755)
        shutil.copy2(lock_file, staging / "appimage-tools.json")
        # copytree preserves the TemporaryDirectory's mode (0700) unless reset.
        # The builder installs as root but CI/package-linux runs as the caller UID.
        staging.chmod(0o755)
        shutil.copytree(staging, destination, dirs_exist_ok=True)
    print(f"Prepared pinned AppImage tools in {destination}")


def verify_tools(directory=TOOLS_DIR, lock_file=LOCK_FILE):
    lock = read_lock(lock_file)
    for relative, artifact in tool_files(lock):
        verify_file(directory / relative, artifact)
    prepared_lock = directory / "appimage-tools.json"
    if not prepared_lock.is_file() or json.loads(prepared_lock.read_text()) != lock:
        raise ValueError("AppImage tool pins changed; rebuild the builder image")
    if not os.access(directory / "appimagetool-x86_64.AppImage", os.X_OK):
        raise ValueError("Pinned appimagetool is not executable; rebuild the builder image")
    return lock


def create_appdir(payload, appdir):
    # Keep the existing bin/lib/share layout: RUNPATH and asset lookup already
    # make it relocatable. No second dependency collector or loader wrapper.
    shutil.copytree(payload, appdir, symlinks=True)
    for name, target in APPDIR_LINKS.items():
        (appdir / name).symlink_to(target)
    if not os.access(appdir / "AppRun", os.X_OK):
        raise ValueError("AppDir entry point is not executable")
    for name in APPDIR_LINKS:
        if not (appdir / name).is_file():
            raise ValueError(f"Missing AppDir metadata target: {name}")
    return appdir


def loader_environment():
    env = dict(os.environ)
    # Do not inherit builder libraries or let another AppImage redirect our
    # pinned runtime/tool or change its extraction/cleanup behavior.
    for name in ("LD_LIBRARY_PATH", "LD_PRELOAD", "LD_AUDIT", "TARGET_APPIMAGE",
                 "APPIMAGE_EXTRACT_AND_RUN", "NO_CLEANUP"):
        env.pop(name, None)
    return env


def github_update_metadata(repository, tag, filename):
    repository = repository.removeprefix("https://github.com/").rstrip("/").removesuffix(".git")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9][A-Za-z0-9_.-]*", repository):
        raise ValueError("AppImage update repository must be a GitHub owner/repository or HTTPS repository URL")
    owner, name = repository.split("/")
    return {
        "information": f"gh-releases-zsync|{owner}|{name}|latest|tiny-player-*-linux-x86_64.AppImage.zsync",
        "file_url": f"https://github.com/{repository}/releases/download/{quote(tag, safe='')}/{quote(filename, safe='')}",
    }


def verify_zsync(image, expected_url=None):
    control = image.with_name(image.name + ".zsync")
    if not control.is_file() or control.stat().st_size == 0:
        raise ValueError("appimagetool did not produce a zsync file; install zsyncmake or rebuild the builder image")
    with control.open("rb") as stream:
        header, separator, _ = stream.read(64 * 1024).partition(b"\n\n")
    if not separator:
        raise ValueError("Invalid zsync header")
    headers = {}
    for line in header.decode("ascii").splitlines():
        name, separator, value = line.partition(": ")
        if not separator or name in headers:
            raise ValueError("Invalid or duplicate zsync header")
        headers[name] = value
    if not headers.get("zsync") or headers.get("Filename") != image.name:
        raise ValueError("zsync filename does not match the AppImage")
    url = headers.get("URL", "")
    parsed_url = urlsplit(url)
    if (parsed_url.scheme != "https" or not parsed_url.netloc
            or unquote(parsed_url.path.rsplit("/", 1)[-1]) != image.name
            or (expected_url is not None and url != expected_url)):
        raise ValueError("zsync URL does not match the AppImage release URL")
    length = int(headers.get("Length", "-1"))
    if length != image.stat().st_size:
        raise ValueError("zsync length does not match the AppImage")
    with image.open("rb") as stream:
        digest = hashlib.file_digest(stream, lambda: hashlib.sha1(usedforsecurity=False)).hexdigest()
    if headers.get("SHA-1") != digest:
        raise ValueError("zsync SHA-1 does not match the AppImage")
    blocksize = int(headers.get("Blocksize", "0"))
    hash_lengths = [int(value) for value in headers.get("Hash-Lengths", "").split(",")]
    if (blocksize <= 0 or blocksize & (blocksize - 1) or len(hash_lengths) != 3
            or hash_lengths[0] < 1 or not 1 <= hash_lengths[1] <= 4 or not 1 <= hash_lengths[2] <= 16):
        raise ValueError("Invalid zsync checksum parameters")
    expected_bytes = ((length + blocksize - 1) // blocksize) * sum(hash_lengths[1:])
    if control.stat().st_size - len(header) - 2 != expected_bytes:
        raise ValueError("Truncated or invalid zsync checksum table")
    return control


def build_appimage(appdir, output, tools=TOOLS_DIR, *, update_information, file_url):
    appdir, output, tools = appdir.resolve(), output.resolve(), tools.resolve()
    env = dict(loader_environment(), ARCH="x86_64")
    # VERSION makes appimagetool replace the root desktop symlink with a file.
    # The explicit artifact name and manifest already carry our application version.
    env.pop("VERSION", None)
    subprocess.run([
        str(tools / "appimagetool-x86_64.AppImage"), "--appimage-extract-and-run",
        "--runtime-file", str(tools / "runtime-x86_64"), "--no-appstream",
        "--updateinformation", update_information, "--file-url", file_url,
        str(appdir), str(output),
    ], check=True, env=env, cwd=output.parent)
    if not output.is_file() or output.stat().st_size == 0:
        raise ValueError("appimagetool did not produce an AppImage")
    output.chmod(0o755)
    return verify_zsync(output, expected_url=file_url)


def extract_appimage(image, destination):
    # This runs only the pinned runtime's extractor, never the player/AppRun,
    # and does not need /dev/fuse or a privileged container.
    destination.mkdir(parents=True)
    subprocess.run(
        [str(image), "--appimage-extract"], cwd=destination,
        env=loader_environment(), check=True, stdout=subprocess.DEVNULL,
    )
    root = destination / "squashfs-root"
    for name, target in APPDIR_LINKS.items():
        path = root / name
        if not path.is_symlink() or os.readlink(path) != target or not path.is_file():
            raise ValueError(f"AppImage did not preserve the AppDir link: {name}")
    return root


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    operation = parser.add_mutually_exclusive_group(required=True)
    operation.add_argument("--prepare-tools", type=Path)
    operation.add_argument("--verify-tools", type=Path)
    operation.add_argument("--verify-zsync", type=Path, metavar="APPIMAGE")
    arguments = parser.parse_args()
    if arguments.prepare_tools is not None:
        prepare_tools(arguments.prepare_tools.resolve())
    elif arguments.verify_tools is not None:
        verify_tools(arguments.verify_tools.resolve())
    else:
        control = verify_zsync(arguments.verify_zsync.resolve())
        print(f"Verified {control}")
