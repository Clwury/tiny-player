import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
NATIVE_PKG_CONFIG = "/opt/tiny-player/lib/pkgconfig:/opt/tiny-player/share/pkgconfig"


@unittest.skipUnless(
    sys.platform.startswith("linux") and platform.machine() == "x86_64" and shutil.which("cargo"),
    "requires Linux x86_64 and Cargo",
)
class LinuxPackageConfigTests(unittest.TestCase):
    def run_package_with_host_config(self, config_name):
        with tempfile.TemporaryDirectory(prefix="tiny-package-config-") as directory:
            root = Path(directory)
            checkout = root / "checkout with spaces"
            (checkout / "scripts").mkdir(parents=True)
            (checkout / ".cargo").mkdir()
            shutil.copy2(ROOT / "scripts/package-linux.sh", checkout / "scripts/package-linux.sh")
            config = checkout / ".cargo" / config_name
            original = (
                '[env]\n'
                'PKG_CONFIG_PATH = { value = "/host-only-sdk/pkgconfig", force = true }\n'
                'LD_LIBRARY_PATH = { value = "/host-only-sdk/lib", force = true }\n'
                'TINY_LIBPLACEBO_LIB_DIR = { value = "/host-only-sdk/lib", force = true }\n'
            )
            config.write_text(original)
            (checkout / "Cargo.toml").write_text(
                '[package]\nname = "package-config-probe"\nversion = "0.0.0"\nedition = "2024"\n',
            )
            (checkout / "Cargo.lock").write_text(
                'version = 4\n\n[[package]]\nname = "package-config-probe"\nversion = "0.0.0"\n',
            )
            (checkout / "src").mkdir()
            (checkout / "src/main.rs").write_text("fn main() {}\n")
            (checkout / "build.rs").write_text('''
fn main() {
    let observed = ["PKG_CONFIG_PATH", "TINY_LIBPLACEBO_LIB_DIR", "LD_LIBRARY_PATH"]
        .map(|name| std::env::var(name).unwrap_or_default()).join("\\n");
    std::fs::write(std::env::var("PROBE_CAPTURE").unwrap(), observed).unwrap();
}
''')
            tools = root / "tools"
            tools.mkdir()
            # Only the container boundary and artifact assembly are substituted.
            # The production shell script invokes real Cargo and rustc against
            # the probe crate, with the same mounts, cwd and SDK environment.
            engine = tools / "container-engine"
            engine.write_text(f"#!{sys.executable}\n" + '''
import os, shlex, subprocess, sys
arguments = sys.argv[1:]
assert arguments[0] == "run", arguments
mounts = {}
for index, argument in enumerate(arguments):
    if argument == "--mount":
        fields = dict(field.split("=", 1) for field in arguments[index + 1].split(",") if "=" in field)
        mounts[fields["dst"]] = fields["src"]
# In Docker /build is a sibling of /src, even though its backing directory
# lives under the host checkout. Keep those ancestors separate in this probe.
mounts["/build"] = os.environ["PROBE_BUILD"]
os.makedirs(mounts["/build"], exist_ok=True)
workdir = arguments[arguments.index("--workdir") + 1] if "--workdir" in arguments else "/src"
body = sys.stdin.read()
for destination, source in mounts.items():
    body = body.replace(destination, shlex.quote(source))
environment = os.environ.copy()
environment.update(
    CARGO_HOME=mounts["/build"] + "/cargo",
    CARGO_TARGET_DIR=mounts["/build"] + "/target",
    CARGO_NET_OFFLINE="true",
    PKG_CONFIG_PATH="/opt/tiny-player/lib/pkgconfig:/opt/tiny-player/share/pkgconfig",
    LD_LIBRARY_PATH="/opt/tiny-player/lib",
)
environment.pop("TINY_LIBPLACEBO_LIB_DIR", None)
result = subprocess.run(["bash", "-s"], input=body, text=True, cwd=mounts[workdir], env=environment)
sys.exit(result.returncode)
''')
            engine.chmod(0o755)
            getconf = tools / "getconf"
            getconf.write_text("#!/bin/sh\nprintf 'glibc 2.39\\n'\n")
            getconf.chmod(0o755)
            python = tools / "python3"
            python.write_text(f"#!{sys.executable}\n" + '''
import sys
assert any(argument.endswith(("/packaging/linux/appimage.py", "/packaging/linux/bundle.py"))
           for argument in sys.argv[1:]), sys.argv
''')
            python.chmod(0o755)
            environment = os.environ.copy()
            environment.update(
                PATH=str(tools) + os.pathsep + environment["PATH"],
                CONTAINER_ENGINE=str(engine),
                PROBE_CAPTURE=str(root / "observed"),
                PROBE_BUILD=str(root / "container-build"),
            )
            result = subprocess.run(
                ["bash", str(checkout / "scripts/package-linux.sh"), "--skip-image-build"],
                cwd=root, env=environment, capture_output=True, text=True, timeout=60,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            observed = (root / "observed").read_text().split("\n")
            self.assertEqual(observed[0], NATIVE_PKG_CONFIG)
            self.assertEqual(observed[1], "")
            self.assertNotIn("/host-only-sdk", observed[2])
            self.assertEqual(config.read_text(), original)

    def test_packaging_ignores_forced_host_sdk_config_toml(self):
        self.run_package_with_host_config("config.toml")

    def test_packaging_ignores_legacy_host_cargo_config(self):
        self.run_package_with_host_config("config")


if __name__ == "__main__":
    unittest.main()
