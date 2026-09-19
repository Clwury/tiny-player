"""Cargo runner: supply local SDK DLLs without changing the user's PATH."""

import json
import os
from pathlib import Path
import subprocess
import sys


def main():
    paths_file = Path(__file__).resolve().parents[2] / "target/windows-x86_64/paths.json"
    if not paths_file.is_file():
        raise SystemExit("Windows SDK missing. Run .\\scripts\\package-windows.ps1 -Mode Prepare first.")
    if len(sys.argv) < 2:
        raise SystemExit("Expected a Cargo executable and optional arguments.")
    paths = json.loads(paths_file.read_text(encoding="utf-8"))
    environment = os.environ.copy()
    environment["PATH"] = os.pathsep.join([
        str(Path(paths["ffmpeg"]) / "bin"),
        str(Path(paths["msys"]) / "bin"),
        environment.get("PATH", ""),
    ])
    try:
        # An argument list preserves spaces/quotes and forwards test harness flags.
        return subprocess.call(sys.argv[1:], env=environment)
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    sys.exit(main())
