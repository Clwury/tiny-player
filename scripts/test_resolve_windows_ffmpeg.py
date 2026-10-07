import hashlib
import json
from pathlib import Path
import tempfile
import unittest
import zipfile

import resolve_windows_ffmpeg as resolver


class LatestFfmpegTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.downloads = self.root / "latest"
        self.downloads.mkdir()
        self.output = self.root / "sdk/native-lock.json"
        self.recipe_commit = "a" * 40

    def archive(self, directories=None, version="n9.0.1-100-g123abcd-20261006"):
        if directories is None:
            directories = (resolver.ARCHIVE.removesuffix(".zip"),)
        archive = self.downloads / resolver.ARCHIVE
        with zipfile.ZipFile(archive, "w") as output:
            for directory in directories:
                output.writestr(f"{directory}/include/libavutil/ffversion.h", f'#define FFMPEG_VERSION "{version}"\n')
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        (self.downloads / "checksums.sha256").write_text(f"{digest}  {archive.name}\n")
        return archive, digest

    def test_latest_sdk_preserves_other_pins_and_records_actual_sources(self):
        archive, digest = self.archive()
        original_path = resolver.ROOT / "packaging/windows/native-lock.json"
        original_bytes = original_path.read_bytes()
        original = json.loads(original_bytes)
        self.assertEqual(resolver.resolve(self.downloads, self.output, self.recipe_commit), digest)
        resolved = json.loads(self.output.read_text())
        self.assertEqual(resolved["ffmpeg"]["version"], "n9.0.1-100-g123abcd-20261006")
        self.assertEqual(resolved["ffmpeg"]["source_url"], "https://github.com/FFmpeg/FFmpeg/tree/123abcd")
        self.assertTrue(resolved["ffmpeg"]["build_source"].endswith(self.recipe_commit))
        for key in ("tools", "packages", "target", "python"):
            self.assertEqual(resolved[key], original[key])
        self.assertEqual(original_path.read_bytes(), original_bytes)
        self.assertEqual((self.output.parent / "downloads" / archive.name).read_bytes(), archive.read_bytes())

    def test_checksum_mismatch_rejects_download_before_creating_lock(self):
        archive, _ = self.archive()
        archive.write_bytes(archive.read_bytes() + b"changed")
        with self.assertRaisesRegex(ValueError, "SHA256 verification"):
            resolver.resolve(self.downloads, self.output, self.recipe_commit)
        self.assertFalse(self.output.exists())

    def test_missing_or_duplicate_checksum_is_rejected(self):
        self.archive()
        checksums = self.downloads / "checksums.sha256"
        valid = checksums.read_text()
        for contents in ("", valid * 2):
            with self.subTest(contents=contents):
                checksums.write_text(contents)
                with self.assertRaisesRegex(ValueError, "exactly one"):
                    resolver.resolve(self.downloads, self.output, self.recipe_commit)

    def test_incompatible_or_ambiguous_archive_is_rejected(self):
        for directories in (("ffmpeg-n8.1-100-g123abcd-win64-lgpl-shared-8.1",), ("first", "second")):
            with self.subTest(directories=directories):
                self.archive(directories)
                with self.assertRaises(ValueError):
                    resolver.resolve(self.downloads, self.output, self.recipe_commit)
                self.assertFalse(self.output.exists())

    def test_invalid_build_recipe_commit_is_rejected(self):
        self.archive()
        with self.assertRaisesRegex(ValueError, "full Git commit"):
            resolver.resolve(self.downloads, self.output, "latest")

    def test_incompatible_version_header_is_rejected(self):
        self.archive(version="n10.0-100-g123abcd-20261006")
        with self.assertRaisesRegex(ValueError, "versioned FFmpeg 9.0"):
            resolver.resolve(self.downloads, self.output, self.recipe_commit)
        self.assertFalse(self.output.exists())


if __name__ == "__main__":
    unittest.main()
