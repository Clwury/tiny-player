import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[2]


@unittest.skipUnless(shutil.which("makepkg") and shutil.which("git"), "needs makepkg and git")
@unittest.skipIf(os.geteuid() == 0, "makepkg refuses to run as root")
class MakepkgSourceTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="tiny-player-makepkg-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.upstream = self.root / "upstream repository"
        self.upstream.mkdir()
        self.git("init", "--initial-branch=main")
        (self.upstream / "Cargo.toml").write_text('[package]\nname = "tiny-player"\nversion = "0.1.1"\n')
        (self.upstream / "src").mkdir()
        (self.upstream / "src/main.rs").write_text("fn main() {}\n")
        self.git("add", ".")
        self.git("-c", "user.name=Packaging Test", "-c", "user.email=test@example.invalid",
                 "-c", "commit.gpgsign=false", "commit", "-m", "Initial test source")
        self.checkout = self.root / "checkout with spaces"
        self.checkout.mkdir()
        (self.checkout / "src").mkdir()
        self.sentinel = self.checkout / "src/main.rs"
        self.sentinel.write_text("// Uncommitted checkout source must survive makepkg.\n")
        pkgbuild = (REPO / "PKGBUILD").read_text().replace(
            "git+https://github.com/Clwury/tiny-player.git", "git+" + self.upstream.as_uri(),
        )
        (self.checkout / "PKGBUILD").write_text(pkgbuild)
        self.environment = os.environ.copy()
        for name in ("BUILDDIR", "SRCDEST", "PKGDEST", "SRCPKGDEST", "LOGDEST",
                     "MAKEPKG_CONF", "BASH_ENV", "GIT_DIR", "GIT_WORK_TREE"):
            self.environment.pop(name, None)
        self.config = self.root / "makepkg.conf"
        self.config.write_text(
            'source /etc/makepkg.conf\n'
            'BUILDDIR="$startdir"\nSRCDEST="$startdir"\nPKGDEST="$startdir"\n'
            'SRCPKGDEST="$startdir"\nLOGDEST="$startdir"\n'
        )

    def git(self, *arguments):
        return subprocess.run(["git", "-C", str(self.upstream), *arguments],
                              check=True, capture_output=True, text=True)

    def makepkg(self, *arguments):
        result = subprocess.run(
            ["makepkg", "--config", str(self.config), "--nocolor", "--holdver", *arguments],
            cwd=self.checkout, env=self.environment, capture_output=True, text=True, timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def assert_checkout_preserved(self):
        self.assertEqual(self.sentinel.read_text(), "// Uncommitted checkout source must survive makepkg.\n")

    def test_first_source_download_creates_redirected_cache_and_can_be_repeated(self):
        cache = self.checkout / ".makepkg/sources/tiny-player"
        self.assertFalse(cache.exists())
        self.makepkg("--verifysource")
        self.assertTrue((cache / "HEAD").is_file())
        self.assertEqual(subprocess.check_output(
            ["git", "--git-dir", str(cache), "rev-parse", "HEAD"], text=True,
        ), self.git("rev-parse", "HEAD").stdout)
        self.makepkg("--verifysource")
        self.assert_checkout_preserved()

    def test_clean_source_preparation_preserves_rust_checkout(self):
        self.makepkg("--nobuild", "--noprepare", "--nodeps", "--cleanbuild")
        working = self.checkout / ".makepkg/tiny-player-git/src/tiny-player"
        self.assertEqual((working / "src/main.rs").read_text(), "fn main() {}\n")
        self.assert_checkout_preserved()
        (working.parent / "stale-build-file").write_text("remove during a clean build")
        self.makepkg("--nobuild", "--noprepare", "--nodeps", "--cleanbuild")
        self.assertFalse((working.parent / "stale-build-file").exists())
        self.assert_checkout_preserved()

    def test_configured_source_cache_is_respected(self):
        cache = self.root / "configured source cache"
        with self.config.open("a") as config:
            config.write(f'SRCDEST="{cache}"\n')
        self.makepkg("--verifysource")
        self.assertTrue((cache / "tiny-player/HEAD").is_file())
        self.assertFalse((self.checkout / ".makepkg/sources").exists())
        self.assert_checkout_preserved()


if __name__ == "__main__":
    unittest.main()
