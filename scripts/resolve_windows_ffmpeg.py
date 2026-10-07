"""Verify BtbN's latest FFmpeg 9.0 download and write a build-local native lock."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import zipfile

ROOT = Path(__file__).resolve().parents[1]
ARCHIVE = "ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip"


def resolve(downloads, output, build_commit):
    if not re.fullmatch(r"[0-9a-f]{40}", build_commit):
        raise ValueError("Expected the FFmpeg-Builds recipe's full Git commit")
    archive = downloads / ARCHIVE
    checksums = (downloads / "checksums.sha256").read_text(encoding="utf-8")
    expected = []
    for line in checksums.splitlines():
        fields = line.split()
        if len(fields) == 2 and fields[1].lstrip("*") == ARCHIVE:
            expected.append(fields[0])
    if len(expected) != 1 or not re.fullmatch(r"[0-9a-f]{64}", expected[0]):
        raise ValueError("Latest FFmpeg archive must have exactly one valid SHA256 entry")
    with archive.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    if digest != expected[0]:
        raise ValueError("Latest FFmpeg archive failed SHA256 verification")
    with zipfile.ZipFile(archive) as source:
        roots = {name.split("/", 1)[0] for name in source.namelist()}
        if roots != {ARCHIVE.removesuffix(".zip")}:
            raise ValueError("Expected a single FFmpeg 9.0 x64 LGPL shared SDK directory")
        try:
            header = source.read(f"{roots.pop()}/include/libavutil/ffversion.h").decode("utf-8")
        except KeyError as error:
            raise ValueError("Expected the SDK's generated FFmpeg version header") from error
    # The floating archive directory contains 'latest'; ffversion.h records the
    # actual upstream version, Git commit and BtbN build date.
    match = re.search(r'^#define\s+FFMPEG_VERSION\s+"(n9\.0(?:\.\d+)?-[0-9]+-g([0-9a-f]+)(?:-[0-9]{8})?)"\s*$', header, re.MULTILINE)
    if not match:
        raise ValueError("Expected a versioned FFmpeg 9.0 x64 LGPL shared SDK")
    lock = json.loads((ROOT / "packaging/windows/native-lock.json").read_text(encoding="utf-8"))
    lock["ffmpeg"] = dict(
        name="ffmpeg", version=match[1], filename=ARCHIVE,
        url=f"https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/{ARCHIVE}",
        sha256=digest,
        build_source=f"https://github.com/BtbN/FFmpeg-Builds/tree/{build_commit}",
        source_url=f"https://github.com/FFmpeg/FFmpeg/tree/{match[2]}",
    )
    # prepare.py reuses this already verified download; no second floating-URL fetch.
    cached = output.parent / "downloads" / ARCHIVE
    cached.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(archive, cached)
    output.write_text(json.dumps(lock, indent=2) + "\n", encoding="utf-8")
    return digest


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--downloads", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--build-commit", required=True)
    args = parser.parse_args()
    digest = resolve(args.downloads, args.output, args.build_commit)
    if "GITHUB_OUTPUT" in os.environ:
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
            output.write(f"sha256={digest}\n")
    print(f"Latest FFmpeg verified; build-local native lock: {args.output}")
