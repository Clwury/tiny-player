import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import appimage
import bundle


REPO = Path(__file__).resolve().parents[2]
SMOKE_TOOLS = Path(os.environ.get("TINY_TEST_APPIMAGE_TOOLS", str(appimage.TOOLS_DIR)))


def make_payload(root):
    for directory in ("bin", "lib", "build-info", "licenses", "share/applications",
                      "share/icons/hicolor/256x256/apps", "share/tiny-player/assets/icons"):
        (root / directory).mkdir(parents=True)
    binary = root / "bin/tiny-player"
    binary.write_text('#!/bin/sh\nprintf "%s\\n" "$1"\n')
    binary.chmod(0o755)
    shutil.copy2(REPO / "tiny-player.desktop", root / "share/applications/tiny-player.desktop")
    shutil.copy2(REPO / "assets/icons/tiny-player.png",
                 root / "share/icons/hicolor/256x256/apps/tiny-player.png")
    (root / "share/tiny-player/assets/icons/probe.txt").write_text("portable asset")


def make_zsync_fixture(image, url):
    # Metadata/publication fixtures only; the smoke test uses real zsyncmake
    # checksums and reconstructs the image with the zsync client.
    data = image.read_bytes()
    header = (
        f"zsync: 0.6.2\nFilename: {image.name}\nBlocksize: 2048\nLength: {len(data)}\n"
        f"Hash-Lengths: 2,2,4\nURL: {url}\n"
        f"SHA-1: {hashlib.sha1(data, usedforsecurity=False).hexdigest()}\n\n"
    ).encode("ascii")
    control = image.with_name(image.name + ".zsync")
    control.write_bytes(header + b"\x00" * (((len(data) + 2047) // 2048) * 6))
    return control


class AppImageTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.payload = self.root / "payload"
        make_payload(self.payload)
        self.update = appimage.github_update_metadata("Example/tiny-player", "v1.2.3", "player.AppImage")

    def test_relative_links_preserve_payload_and_arguments_after_moving(self):
        source_hash = appimage.sha256(self.payload / "bin/tiny-player")
        appdir = appimage.create_appdir(self.payload, self.root / "TinyPlayer.AppDir")
        moved = self.root / "移动后的 AppDir"
        appdir.rename(moved)
        for name, target in appimage.APPDIR_LINKS.items():
            self.assertTrue((moved / name).is_symlink())
            self.assertEqual(os.readlink(moved / name), target)
            self.assertTrue((moved / name).is_file())
        self.assertEqual(appimage.sha256(moved / "bin/tiny-player"), source_hash)
        self.assertFalse((self.payload / "AppRun").exists())
        result = subprocess.check_output(
            [str(moved / "AppRun"), "参数 with spaces"], cwd=self.root, text=True,
        )
        self.assertEqual(result, "参数 with spaces\n")

    def test_missing_metadata_and_non_executable_entry_point_are_rejected(self):
        (self.payload / "bin/tiny-player").chmod(0o644)
        with self.assertRaisesRegex(ValueError, "entry point"):
            appimage.create_appdir(self.payload, self.root / "not-executable")
        (self.payload / "bin/tiny-player").chmod(0o755)
        (self.payload / "share/applications/tiny-player.desktop").unlink()
        with self.assertRaisesRegex(ValueError, "metadata target"):
            appimage.create_appdir(self.payload, self.root / "missing-desktop")

    def test_packaging_uses_explicit_runtime_and_no_fuse_or_builder_loader(self):
        appdir = appimage.create_appdir(self.payload, self.root / "AppDir with spaces")
        output = self.root / "player.AppImage"
        tools = self.root / "tools"

        def pack(command, **kwargs):
            self.assertEqual(command, [
                str(tools / "appimagetool-x86_64.AppImage"), "--appimage-extract-and-run",
                "--runtime-file", str(tools / "runtime-x86_64"), "--no-appstream",
                "--updateinformation", self.update["information"], "--file-url", self.update["file_url"],
                str(appdir), str(output),
            ])
            self.assertTrue(kwargs["check"])
            self.assertEqual(kwargs["cwd"], output.parent)
            self.assertEqual(kwargs["env"]["ARCH"], "x86_64")
            self.assertNotIn("VERSION", kwargs["env"])
            for name in ("LD_LIBRARY_PATH", "LD_PRELOAD", "LD_AUDIT", "TARGET_APPIMAGE",
                         "APPIMAGE_EXTRACT_AND_RUN", "NO_CLEANUP"):
                self.assertNotIn(name, kwargs["env"])
            output.write_bytes(b"fixture AppImage")
            make_zsync_fixture(output, self.update["file_url"])

        with patch.dict(os.environ, LD_LIBRARY_PATH="/builder/lib", LD_PRELOAD="builder.so",
                        LD_AUDIT="audit.so", VERSION="unrelated", TARGET_APPIMAGE="/other.AppImage",
                        APPIMAGE_EXTRACT_AND_RUN="1", NO_CLEANUP="1"), \
                patch.object(appimage.subprocess, "run", side_effect=pack):
            control = appimage.build_appimage(
                appdir, output, tools, update_information=self.update["information"],
                file_url=self.update["file_url"],
            )
        self.assertEqual(output.stat().st_mode & 0o777, 0o755)
        self.assertEqual(control, output.with_name(output.name + ".zsync"))

    def test_packaging_failure_or_missing_output_is_not_accepted(self):
        output = self.root / "player.AppImage"
        with patch.object(appimage.subprocess, "run", side_effect=subprocess.CalledProcessError(1, "tool")):
            with self.assertRaises(subprocess.CalledProcessError):
                appimage.build_appimage(self.payload, output, update_information=self.update["information"],
                                       file_url=self.update["file_url"])
        self.assertFalse(output.exists())
        with patch.object(appimage.subprocess, "run"):
            with self.assertRaisesRegex(ValueError, "did not produce"):
                appimage.build_appimage(self.payload, output, update_information=self.update["information"],
                                       file_url=self.update["file_url"])

    def test_successful_tool_without_zsync_output_is_rejected(self):
        image = self.root / "player.AppImage"
        with patch.object(appimage.subprocess, "run", side_effect=lambda *args, **kwargs: image.write_bytes(b"image")):
            with self.assertRaisesRegex(ValueError, "did not produce a zsync file"):
                appimage.build_appimage(self.payload, image, update_information=self.update["information"],
                                       file_url=self.update["file_url"])

    def test_zsync_metadata_matches_the_final_image_and_release_url(self):
        image = self.root / "player.AppImage"
        image.write_bytes(b"fixture AppImage")
        control = make_zsync_fixture(image, self.update["file_url"])
        original = control.read_bytes()
        self.assertEqual(appimage.verify_zsync(image, self.update["file_url"]), control)
        for old, new, message in (
            (b"Filename: player.AppImage", b"Filename: other.AppImage", "filename"),
            (b"Length: 16", b"Length: 17", "length"),
            (self.update["file_url"].encode(), b"https://example.invalid/player.AppImage", "URL"),
        ):
            with self.subTest(message=message):
                self.assertIn(old, original)
                control.write_bytes(original.replace(old, new))
                with self.assertRaisesRegex(ValueError, message):
                    appimage.verify_zsync(image, self.update["file_url"])
        control.write_bytes(original[:-1])
        with self.assertRaisesRegex(ValueError, "checksum table"):
            appimage.verify_zsync(image)
        control.write_bytes(original)
        image.write_bytes(b"changed AppImage")  # Same size; must detect a stale control file.
        with self.assertRaisesRegex(ValueError, "SHA-1"):
            appimage.verify_zsync(image)

    def test_update_metadata_uses_latest_feed_and_versioned_download_url(self):
        for repository in ("Clwury/tiny-player", "https://github.com/Clwury/tiny-player.git"):
            metadata = appimage.github_update_metadata(repository, "v1.2.3+build", "player.AppImage")
            self.assertEqual(metadata["information"],
                             "gh-releases-zsync|Clwury|tiny-player|latest|tiny-player-*-linux-x86_64.AppImage.zsync")
            self.assertEqual(metadata["file_url"],
                             "https://github.com/Clwury/tiny-player/releases/download/v1.2.3%2Bbuild/player.AppImage")
        for repository in ("invalid", "https://example.invalid/owner/repo", "owner/repo|other", "../repo"):
            with self.subTest(repository=repository), self.assertRaises(ValueError):
                appimage.github_update_metadata(repository, "v1.2.3", "player.AppImage")

    def test_extraction_checks_links_without_running_the_player(self):
        appdir = appimage.create_appdir(self.payload, self.root / "TinyPlayer.AppDir")
        image = self.root / "fixture.AppImage"
        destination = self.root / "extracted"

        def extract(command, **kwargs):
            self.assertEqual(command, [str(image), "--appimage-extract"])
            self.assertEqual(kwargs["cwd"], destination)
            self.assertEqual(kwargs["stdout"], subprocess.DEVNULL)
            shutil.copytree(appdir, destination / "squashfs-root", symlinks=True)

        with patch.object(appimage.subprocess, "run", side_effect=extract):
            self.assertEqual(appimage.extract_appimage(image, destination), destination / "squashfs-root")
        (appdir / "AppRun").unlink()
        (appdir / "AppRun").symlink_to("share/applications/tiny-player.desktop")
        destination = self.root / "tampered"
        with patch.object(appimage.subprocess, "run", side_effect=extract):
            with self.assertRaisesRegex(ValueError, "AppRun"):
                appimage.extract_appimage(image, destination)

    def test_checksums_use_the_final_artifact_name(self):
        image = self.root / "tiny-player-1.2.3-linux-x86_64.AppImage"
        image.write_bytes(b"fixture")
        checksum = bundle.write_checksum(image)
        self.assertEqual(checksum.name, image.name + ".sha256")
        self.assertEqual(checksum.read_text(), f"{hashlib.sha256(b'fixture').hexdigest()}  {image.name}\n")
        subprocess.run(["sha256sum", "--check", "--strict", checksum.name], cwd=self.root, check=True)


class ToolFixture:
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.directory = self.root / "prepared tools"
        self.lock_file = self.root / "appimage-tools.json"
        self.contents = {
            "appimagetool": b"fixture tool",
            "runtime": b"fixture runtime",
            "runtime_license": b"fixture runtime license",
            "musl": b"fixture musl license",
        }
        self.lock = {"architecture": "x86_64"}
        for name, content in self.contents.items():
            entry = {"url": f"https://example.invalid/{name}",
                     "sha256": hashlib.sha256(content).hexdigest()}
            if name == "musl":
                self.lock["third_party_licenses"] = [dict(entry, name=name)]
            else:
                self.lock[name] = entry
        self.write_lock()

    def write_lock(self):
        self.lock_file.write_text(json.dumps(self.lock))

    def fake_download(self, command, **kwargs):
        self.assertTrue(kwargs["check"])
        path = Path(command[command.index("--output") + 1])
        path.write_bytes(self.contents[command[-1].rsplit("/", 1)[1]])

    def prepare(self):
        with patch.object(appimage.subprocess, "run", side_effect=self.fake_download):
            appimage.prepare_tools(self.directory, self.lock_file)

class ToolPinTests(ToolFixture, unittest.TestCase):
    def test_prepare_verifies_all_downloads_and_preserves_metadata(self):
        self.prepare()
        self.assertEqual(appimage.verify_tools(self.directory, self.lock_file), self.lock)
        for relative, entry in appimage.tool_files(self.lock):
            self.assertEqual(appimage.sha256(self.directory / relative), entry["sha256"])
        self.assertEqual((self.directory / "runtime-x86_64").stat().st_mode & 0o777, 0o755)
        self.assertEqual(self.directory.stat().st_mode & 0o777, 0o755)

    def test_checksum_mismatch_never_publishes_downloads(self):
        self.lock["runtime"]["sha256"] = "0" * 64
        self.write_lock()
        with patch.object(appimage.subprocess, "run", side_effect=self.fake_download):
            with self.assertRaisesRegex(ValueError, "mismatched"):
                appimage.prepare_tools(self.directory, self.lock_file)
        self.assertFalse(self.directory.exists())
        self.assertEqual(list(self.root.glob("appimage-tools-*")), [])

    def test_corrupted_files_and_stale_prepared_pins_are_rejected(self):
        self.prepare()
        (self.directory / "runtime-x86_64").write_bytes(b"tampered")
        with self.assertRaisesRegex(ValueError, "rebuild"):
            appimage.verify_tools(self.directory, self.lock_file)
        self.prepare()
        self.lock["runtime"]["release"] = "changed pin"
        self.write_lock()
        with self.assertRaisesRegex(ValueError, "pins changed"):
            appimage.verify_tools(self.directory, self.lock_file)

    def test_bad_pins_or_unsafe_license_names_fail_before_download(self):
        invalid = [
            ("architecture", "aarch64"),
            ("appimagetool", {"url": "https://example.invalid/tool", "sha256": "bad"}),
            ("runtime", {"url": "http://example.invalid/runtime", "sha256": "0" * 64}),
            ("third_party_licenses", [dict(self.lock["runtime_license"], name="../escape")]),
            ("third_party_licenses", [dict(self.lock["runtime_license"], name="runtime")]),
        ]
        original = dict(self.lock)
        for key, value in invalid:
            with self.subTest(key=key, value=value):
                self.lock = dict(original, **{key: value})
                self.write_lock()
                with patch.object(appimage.subprocess, "run") as download:
                    with self.assertRaises(ValueError):
                        appimage.prepare_tools(self.directory, self.lock_file)
                    download.assert_not_called()

    def test_repository_continuous_snapshots_have_hashes_and_source_commits(self):
        lock = appimage.read_lock()
        for name, repository, filename in (
            ("appimagetool", "appimagetool", "appimagetool-x86_64.AppImage"),
            ("runtime", "type2-runtime", "runtime-x86_64"),
        ):
            entry = lock[name]
            self.assertEqual(entry["release"], "continuous")
            self.assertEqual(entry["url"],
                             f"https://github.com/AppImage/{repository}/releases/download/continuous/{filename}")
            self.assertRegex(entry["sha256"], r"^[0-9a-f]{64}$")
            self.assertRegex(entry["source_commit"], r"^[0-9a-f]{40}$")
        commit = lock["runtime"]["source_commit"]
        self.assertEqual(lock["runtime"]["source_url"],
                         f"https://github.com/AppImage/type2-runtime/tree/{commit}")
        self.assertEqual(lock["runtime"]["build_recipe_url"],
                         f"https://github.com/AppImage/type2-runtime/blob/{commit}/scripts/docker/Dockerfile")
        self.assertEqual(lock["runtime_license"]["url"],
                         f"https://raw.githubusercontent.com/AppImage/type2-runtime/{commit}/LICENSE")


class BundleFormatsTests(ToolFixture, unittest.TestCase):
    def test_both_formats_share_one_audited_payload(self):
        self.check_formats()

    def test_failed_extraction_audit_leaves_existing_outputs_untouched(self):
        self.check_formats(mismatch=True)

    def test_missing_zsync_leaves_existing_outputs_untouched(self):
        self.check_formats(missing_zsync=True)

    def test_update_repository_override_is_used_in_both_formats(self):
        self.check_formats(repository="Example/player-fork")

    def check_formats(self, mismatch=False, missing_zsync=False, repository=None):
        self.prepare()
        output = self.root / "dist"
        output.mkdir()
        sentinel = output / "existing-package"
        sentinel.write_bytes(b"keep")
        elf_manifest = {"bin/tiny-player": {"sha256": "audited payload"}}

        def assemble(binary, repo, root, package):
            make_payload(root)

        def pack(appdir, image, tools, *, update_information, file_url):
            self.assertFalse((appdir / "install.sh").exists())
            self.assertFalse((appdir / "README.txt").exists())
            self.assertEqual(json.loads((appdir / "manifest.json").read_text())["elf_files"], elf_manifest)
            self.assertEqual(json.loads((appdir / "build-info/appimage-tools.json").read_text()), self.lock)
            metadata = json.loads((appdir / "manifest.json").read_text())["appimage_update"]
            self.assertEqual(metadata, {"information": update_information, "file_url": file_url})
            owner, name = (repository or "Clwury/tiny-player").split("/")
            self.assertEqual(update_information,
                             f"gh-releases-zsync|{owner}|{name}|latest|tiny-player-*-linux-x86_64.AppImage.zsync")
            self.assertIn(f"https://github.com/{owner}/{name}/releases/download/v", file_url)
            self.assertTrue(file_url.endswith("/" + image.name))
            image.write_bytes(b"fixture AppImage")
            image.chmod(0o755)
            if not missing_zsync:
                make_zsync_fixture(image, file_url)
            return appimage.verify_zsync(image, file_url)

        def extract(image, destination):
            root = destination / "squashfs-root"
            shutil.copytree(image.parent / "TinyPlayer.AppDir", root, symlinks=True)
            return root

        runtime_info = {"needed": [], "glibc_required": "0.0"}
        audit_results = [elf_manifest, {} if mismatch else elf_manifest]
        with patch.dict(os.environ, TINY_APPIMAGE_REPOSITORY=repository or ""), \
                patch.object(bundle, "run", side_effect=["glibc 2.39", "no interpreter"]), \
                patch.object(bundle, "inspect_elf", return_value=runtime_info), \
                patch.object(appimage, "TOOLS_DIR", self.directory), \
                patch.object(appimage, "verify_tools", return_value=self.lock), \
                patch.object(bundle, "assemble_payload", side_effect=assemble) as assembly, \
                patch.object(bundle, "audit_bundle", side_effect=audit_results) as audit, \
                patch.object(appimage, "build_appimage", side_effect=pack), \
                patch.object(appimage, "extract_appimage", side_effect=extract):
            if missing_zsync:
                with self.assertRaisesRegex(ValueError, "did not produce a zsync file"):
                    bundle.build_bundle(self.root / "binary", output)
            elif mismatch:
                with self.assertRaisesRegex(ValueError, "payload differs"):
                    bundle.build_bundle(self.root / "binary", output)
            else:
                bundle.build_bundle(self.root / "binary", output)
            assembly.assert_called_once()
            self.assertEqual(audit.call_count, 1 if missing_zsync else 2)
        self.assertEqual(sentinel.read_bytes(), b"keep")
        if mismatch or missing_zsync:
            self.assertEqual(list(output.iterdir()), [sentinel])
            return
        version = json.loads(next(output.glob("*.manifest.json")).read_text())["version"]
        stem = f"tiny-player-{version}-linux-x86_64"
        self.assertEqual({p.name for p in output.iterdir()}, {
            "existing-package", stem + ".tar.gz", stem + ".tar.gz.sha256",
            stem + ".AppImage", stem + ".AppImage.sha256", stem + ".AppImage.zsync", stem + ".manifest.json",
        })
        with tarfile.open(output / (stem + ".tar.gz")) as archive:
            self.assertEqual(json.load(archive.extractfile("tiny-player/manifest.json"))["elf_files"], elf_manifest)
            self.assertEqual(archive.getmember("tiny-player/install.sh").mode & 0o777, 0o755)
            self.assertIn("tiny-player/README.txt", archive.getnames())
            self.assertNotIn("tiny-player/AppRun", archive.getnames())
            self.assertIn("tiny-player/licenses/appimage-runtime/runtime.txt", archive.getnames())
        subprocess.run([
            "sha256sum", "--check", "--strict", stem + ".tar.gz.sha256", stem + ".AppImage.sha256",
        ], cwd=output, check=True)
        self.assertEqual(appimage.verify_zsync(output / (stem + ".AppImage")), output / (stem + ".AppImage.zsync"))


class PublicationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.output = self.root / "dist"
        self.output.mkdir()
        source = self.root / "source"
        source.mkdir()
        self.artifacts = []
        for suffix in (".tar.gz", ".tar.gz.sha256", ".AppImage", ".AppImage.sha256", ".AppImage.zsync", ".manifest.json"):
            artifact = source / ("package" + suffix)
            artifact.write_bytes(b"new " + suffix.encode())
            self.artifacts.append(artifact)
        self.artifacts[2].chmod(0o755)
        self.original_replace = os.replace

    def old_outputs(self):
        for artifact in self.artifacts:
            (self.output / artifact.name).write_bytes(b"old " + artifact.name.encode())
        return self.snapshot()

    def snapshot(self):
        return {path.name: (path.read_bytes(), path.stat().st_mode)
                for path in self.output.iterdir() if path.is_file()}

    def fail_activation(self, source, destination):
        if Path(source).parent.name == "new" and str(destination).endswith(".AppImage"):
            raise OSError("activation failed")
        self.original_replace(source, destination)

    def test_complete_set_is_published_with_executable_mode(self):
        self.old_outputs()
        bundle.publish_artifacts(self.artifacts, self.output)
        self.assertEqual({p.name for p in self.output.iterdir()}, {p.name for p in self.artifacts})
        for artifact in self.artifacts:
            self.assertEqual((self.output / artifact.name).read_bytes(), artifact.read_bytes())
        self.assertEqual((self.output / "package.AppImage").stat().st_mode & 0o777, 0o755)

    def test_late_activation_failure_restores_every_previous_artifact(self):
        original = self.old_outputs()
        with patch.object(bundle.os, "replace", side_effect=self.fail_activation):
            with self.assertRaisesRegex(OSError, "activation failed"):
                bundle.publish_artifacts(self.artifacts, self.output)
        self.assertEqual(self.snapshot(), original)
        self.assertEqual(list(self.output.glob(".tiny-player-publish-*")), [])

    def test_zsync_activation_failure_restores_the_matching_appimage_and_control(self):
        original = self.old_outputs()

        def fail_control(source, destination):
            if Path(source).parent.name == "new" and str(destination).endswith(".AppImage.zsync"):
                raise OSError("zsync activation failed")
            self.original_replace(source, destination)

        with patch.object(bundle.os, "replace", side_effect=fail_control):
            with self.assertRaisesRegex(OSError, "zsync activation failed"):
                bundle.publish_artifacts(self.artifacts, self.output)
        self.assertEqual(self.snapshot(), original)
        self.assertEqual(list(self.output.glob(".tiny-player-publish-*")), [])

    def test_copy_failure_does_not_touch_previous_outputs(self):
        original = self.old_outputs()
        copy = shutil.copy2

        def fail_copy(source, destination):
            if str(source).endswith(".AppImage"):
                raise OSError("copy failed")
            return copy(source, destination)

        with patch.object(bundle.shutil, "copy2", side_effect=fail_copy):
            with self.assertRaisesRegex(OSError, "copy failed"):
                bundle.publish_artifacts(self.artifacts, self.output)
        self.assertEqual(self.snapshot(), original)
        self.assertEqual(list(self.output.glob(".tiny-player-publish-*")), [])

    def test_failed_first_publish_removes_partially_activated_outputs(self):
        with patch.object(bundle.os, "replace", side_effect=self.fail_activation):
            with self.assertRaisesRegex(OSError, "activation failed"):
                bundle.publish_artifacts(self.artifacts, self.output)
        self.assertEqual(list(self.output.iterdir()), [])

    def test_failed_backup_does_not_delete_an_untouched_previous_file(self):
        original = self.old_outputs()

        def fail_backup(source, destination):
            if Path(destination).parent.name == "previous" and str(source).endswith(".AppImage"):
                raise OSError("backup failed")
            self.original_replace(source, destination)

        with patch.object(bundle.os, "replace", side_effect=fail_backup):
            with self.assertRaisesRegex(OSError, "backup failed"):
                bundle.publish_artifacts(self.artifacts, self.output)
        self.assertEqual(self.snapshot(), original)

    def test_failed_rollback_preserves_recovery_files(self):
        self.old_outputs()

        def fail_restore(source, destination):
            if Path(source).parent.name == "previous":
                raise OSError("restore failed")
            self.fail_activation(source, destination)

        with patch.object(bundle.os, "replace", side_effect=fail_restore):
            with self.assertRaisesRegex(RuntimeError, "recovery files remain"):
                bundle.publish_artifacts(self.artifacts, self.output)
        recovery = list(self.output.glob(".tiny-player-publish-*/previous/package.AppImage"))
        self.assertEqual(len(recovery), 1)
        self.assertEqual(recovery[0].read_bytes(), b"old package.AppImage")

    def test_existing_output_directory_is_never_replaced(self):
        original = self.old_outputs()
        target = self.output / "package.AppImage"
        target.unlink()
        target.mkdir()
        (target / "unrelated").write_bytes(b"keep")
        with self.assertRaisesRegex(ValueError, "output directory"):
            bundle.publish_artifacts(self.artifacts, self.output)
        self.assertEqual((target / "unrelated").read_bytes(), b"keep")
        original.pop("package.AppImage")
        self.assertEqual(self.snapshot(), original)


@unittest.skipUnless(SMOKE_TOOLS.is_dir() and shutil.which("cc") and shutil.which("patchelf") and shutil.which("zsync"),
                     "needs pinned AppImage tools and ELF build tools (included in the builder)")
class AppImageSmokeTests(unittest.TestCase):
    def test_real_appimage_round_trip_and_no_fuse_run_after_moving(self):
        appimage.verify_tools(SMOKE_TOOLS)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            payload = root / "payload"
            make_payload(payload)
            library = payload / "lib/libexample.so.1"
            subprocess.run([
                "cc", "-shared", "-fPIC", "-x", "c", "-", "-Wl,-soname,libexample.so.1",
                "-o", str(library),
            ], input=b"int example(void) { return 42; }", check=True)
            code = br'''
#include <limits.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
int example(void);
int main(int argc, char **argv) {
    char exe[PATH_MAX], path[PATH_MAX + 64], asset[64] = {0};
    ssize_t length = readlink("/proc/self/exe", exe, sizeof(exe) - 1);
    if (length < 0 || argc != 2) return 1;
    exe[length] = 0;
    *strrchr(exe, '/') = 0;
    snprintf(path, sizeof(path), "%s/../share/tiny-player/assets/icons/probe.txt", exe);
    FILE *file = fopen(path, "r");
    if (!file) return 2;
    if (!fgets(asset, sizeof(asset), file)) return 3;
    fclose(file);
    printf("%s|%s|%d\n", asset, argv[1], example());
    return 0;
}
'''
            subprocess.run([
                "cc", "-x", "c", "-", "-L" + str(payload / "lib"), "-l:libexample.so.1",
                "-o", str(payload / "bin/tiny-player"),
            ], input=code, check=True)
            subprocess.run(["patchelf", "--set-rpath", "$ORIGIN", str(library)], check=True)
            subprocess.run([
                "patchelf", "--set-rpath", "$ORIGIN/../lib", str(payload / "bin/tiny-player"),
            ], check=True)
            manifest = bundle.audit_bundle(payload)
            appdir = appimage.create_appdir(payload, root / "TinyPlayer.AppDir")
            image = root / "fixture.AppImage"
            update = appimage.github_update_metadata("Example/tiny-player", "v1.2.3", image.name)
            control = appimage.build_appimage(
                appdir, image, SMOKE_TOOLS, update_information=update["information"],
                file_url=update["file_url"],
            )
            embedded = subprocess.check_output(
                [str(image), "--appimage-updateinformation"], text=True, env=appimage.loader_environment(),
            ).strip()
            self.assertEqual(embedded, update["information"])
            self.assertEqual(appimage.verify_zsync(image, update["file_url"]), control)
            reconstructed = root / "reconstructed.AppImage"
            # A complete local seed must reconstruct without downloading data.
            subprocess.run([
                "zsync", "-i", str(image), "-o", str(reconstructed), str(control),
            ], check=True, cwd=root, env=appimage.loader_environment(), capture_output=True, timeout=20)
            self.assertEqual(appimage.sha256(reconstructed), appimage.sha256(image))
            extracted = appimage.extract_appimage(image, root / "extracted")
            self.assertEqual(bundle.audit_bundle(extracted), manifest)
            self.assertEqual((extracted / "share/tiny-player/assets/icons/probe.txt").read_text(), "portable asset")
            moved = root / "移动目录 with spaces/播放器.AppImage"
            moved.parent.mkdir()
            image.rename(moved)
            runtime_tmp = root / "runtime-tmp"
            runtime_tmp.mkdir()
            env = dict(appimage.loader_environment(), TMPDIR=str(runtime_tmp))
            env.pop("NO_CLEANUP", None)
            result = subprocess.run([
                str(moved), "--appimage-extract-and-run", "参数 with spaces",
            ], cwd=root, env=env, text=True, capture_output=True, check=True)
            self.assertEqual(result.stdout, "portable asset|参数 with spaces|42\n")


if __name__ == "__main__":
    unittest.main()
