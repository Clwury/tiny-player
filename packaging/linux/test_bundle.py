import fcntl
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import bundle


class ElfAuditTests(unittest.TestCase):
    def test_versions_are_compared_numerically_and_definitions_are_ignored(self):
        output = """Version definition section '.gnu.version_d':
          Name: GLIBC_2.99
        Version needs section '.gnu.version_r':
          Name: GLIBC_2.9
          Name: GLIBC_2.39
          Name: GLIBC_2.2.5
        """
        self.assertEqual(bundle.required_glibc(output), (2, 39))

    def test_private_or_unknown_glibc_abi_is_rejected(self):
        for name in ("GLIBC_PRIVATE", "GLIBC_ABI_FUTURE"):
            with self.subTest(name=name), self.assertRaises(ValueError):
                bundle.required_glibc(f"Version needs section: Name: {name}")

    def test_newer_glibc_and_wrong_architecture_are_rejected(self):
        with patch.object(bundle, "run", side_effect=[
            "Class: ELF64\nMachine: Advanced Micro Devices X86-64",
            "Version needs section: Name: GLIBC_2.40",
        ]), self.assertRaisesRegex(ValueError, "exceeds 2.39"):
            bundle.inspect_elf(Path("too-new"))
        with patch.object(bundle, "run", return_value="Class: ELF64\nMachine: AArch64"), \
                self.assertRaisesRegex(ValueError, "Not an x86_64"):
            bundle.inspect_elf(Path("wrong-architecture"))

    def test_missing_library_or_symbol_version_is_rejected(self):
        for output in ("libavcodec.so.63 => not found", "libc.so.6: version `GLIBC_2.40' not found"):
            with self.subTest(output=output), self.assertRaises(ValueError):
                bundle.parse_ldd(output)

    def test_library_paths_with_spaces_survive_relocation(self):
        libraries = bundle.parse_ldd("  libavcodec.so.63 => /tmp/moved app/lib/libavcodec.so.63 (0x1234)")
        self.assertEqual(libraries["libavcodec.so.63"], Path("/tmp/moved app/lib/libavcodec.so.63"))

    @unittest.skipUnless(shutil.which("cc") and shutil.which("patchelf"), "needs ELF build tools")
    def test_audit_detects_removed_transitive_library(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "moved bundle"
            (root / "bin").mkdir(parents=True)
            (root / "lib").mkdir()
            library = root / "lib/libexample.so.1"
            leaf = root / "lib/libleaf.so.1"
            binary = root / "bin/tiny-player"
            subprocess.run(["cc", "-shared", "-fPIC", "-x", "c", "-", "-Wl,-soname,libleaf.so.1",
                            "-o", str(leaf)], input=b"int leaf(void) { return 0; }", check=True)
            subprocess.run(["cc", "-shared", "-fPIC", "-x", "c", "-", "-Wl,-soname,libexample.so.1",
                            "-L" + str(root / "lib"), "-l:libleaf.so.1", "-o", str(library)],
                           input=b"int leaf(void); int example(void) { return leaf(); }", check=True)
            subprocess.run(["cc", "-x", "c", "-", "-L" + str(root / "lib"), "-l:libexample.so.1",
                            "-Wl,-rpath-link," + str(root / "lib"),
                            "-o", str(binary)], input=b"int example(void); int main(void) { return example(); }", check=True)
            subprocess.run(["patchelf", "--set-rpath", "$ORIGIN", str(leaf)], check=True)
            subprocess.run(["patchelf", "--set-rpath", "$ORIGIN", str(library)], check=True)
            subprocess.run(["patchelf", "--set-rpath", "$ORIGIN/../lib", str(binary)], check=True)
            self.assertEqual(len(bundle.audit_bundle(root)), 3)
            subprocess.run([str(binary)], check=True)
            leaf.unlink()
            with self.assertRaisesRegex(ValueError, "Missing bundled dependency"):
                bundle.audit_bundle(root)


class InstallerTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.prefix = self.root / "install with spaces"

    def make_bundle(self, version):
        source = self.root / f"source bundle {version}"
        for folder in ("bin", "lib", "share/applications", "share/icons/hicolor/scalable/apps",
                       "share/icons/hicolor/256x256/apps"):
            (source / folder).mkdir(parents=True)
        executable = source / "bin/tiny-player"
        executable.write_text(f"#!/bin/sh\nprintf '%s\\n' {shlex.quote(version)}\n")
        executable.chmod(0o755)
        (source / f"lib/version-{version}.so").write_text(version)
        (source / "share/applications/tiny-player.desktop").write_text(
            f"[Desktop Entry]\nType=Application\nName=Tiny Player {version}\nExec=tiny-player\n"
        )
        (source / "share/icons/hicolor/scalable/apps/tiny-player.svg").write_text(
            f'<svg xmlns="http://www.w3.org/2000/svg"><title>{version}</title></svg>\n'
        )
        shutil.copy2(
            Path(__file__).resolve().parents[2] / "assets/icons/tiny-player.png",
            source / "share/icons/hicolor/256x256/apps/tiny-player.png",
        )
        self.write_test_installer(source, self.prefix)
        return source

    def write_test_installer(self, source, installation_dir):
        # Redirect only the filesystem location in the test fixture so transaction
        # tests never touch the real user's installation or need a product override.
        script = Path(__file__).with_name("install.sh").read_text()
        original = 'prefix="$HOME/.local"\n'
        if script.count(original) != 1:
            raise ValueError("Cannot isolate the installer's fixed directory")
        (source / "install.sh").write_text(script.replace(
            original, f"prefix={shlex.quote(str(installation_dir))}\n", 1,
        ))
        (source / "install.sh").chmod(0o755)

    def install(self, source, *, args=(), env=None, check=True):
        # Installer tests must never notify the user's live desktop session.
        install_env = dict(os.environ if env is None else env)
        install_env["DBUS_SESSION_BUS_ADDRESS"] = f"unix:path={self.root}/no-session-bus"
        return subprocess.run(
            ["bash", str(source / "install.sh"), *map(str, args)],
            env=install_env, check=check, capture_output=True, text=True, timeout=10,
        )

    def command_environment(self, command, body):
        commands = self.root / "test commands"
        commands.mkdir(exist_ok=True)
        wrapper = commands / command
        wrapper.write_text(
            "#!/usr/bin/env bash\n" + body +
            f'\nexec {shlex.quote(shutil.which(command))} "$@"\n'
        )
        wrapper.chmod(0o755)
        return dict(os.environ, PATH=str(commands) + os.pathsep + os.environ["PATH"])

    def installed_files(self):
        return {
            str(path.relative_to(self.prefix)): (
                ("link", os.readlink(path)) if path.is_symlink()
                else ("file", path.read_bytes(), path.stat().st_mode)
            )
            for path in self.prefix.rglob("*") if path.is_symlink() or path.is_file()
        }

    def assert_no_staging(self):
        self.assertEqual(list(self.prefix.glob(".tiny-player.installing.*")), [])

    def test_default_installation_uses_current_user_local_directory(self):
        source = self.make_bundle("1.0")
        shutil.copy2(Path(__file__).with_name("install.sh"), source / "install.sh")
        arguments = self.root / "mkdir-arguments.txt"
        env = self.command_environment("mkdir", '''
printf '%s\\n' "$@" > "$TINY_PLAYER_TEST_MKDIR_LOG"
exit 73''')
        env["TINY_PLAYER_TEST_MKDIR_LOG"] = str(arguments)
        result = self.install(source, env=env, check=False)
        self.assertEqual(result.returncode, 73)
        self.assertEqual(arguments.read_text().splitlines(), ["-p", "--", os.environ["HOME"] + "/.local"])

    def test_directory_and_unknown_arguments_are_rejected_before_installing(self):
        source = self.make_bundle("1.0")
        custom_directory = self.root / "custom install"
        for args in ((custom_directory,), ("--prefix", custom_directory),
                     ("--unknown",), ("--help", custom_directory)):
            with self.subTest(args=args):
                result = self.install(source, args=args, check=False)
                self.assertEqual(result.returncode, 2)
                self.assertIn("directory is fixed", result.stderr)
                self.assertFalse(self.prefix.exists())
                self.assertFalse(custom_directory.exists())

    def test_help_describes_fixed_installation_without_writing_files(self):
        source = self.make_bundle("1.0")
        result = self.install(source, args=("--help",))
        self.assertIn("~/.local/tiny-player.app", result.stdout)
        self.assertNotIn("[PREFIX]", result.stdout)
        self.assertFalse(self.prefix.exists())

    def test_install_with_spaces_preserves_source_and_creates_launcher(self):
        source = self.make_bundle("1.0")
        result = self.install(source)
        self.assertIn("Installed Tiny Player", result.stdout)
        link = self.prefix / "bin/tiny-player"
        self.assertEqual(link.resolve(), self.prefix / "tiny-player.app/bin/tiny-player")
        self.assertEqual(subprocess.check_output([str(link)], text=True), "1.0\n")
        self.assertTrue((source / "bin/tiny-player").exists())
        desktop = self.prefix / "share/applications/tiny-player.desktop"
        self.assertIn(f'Exec="{link}"', desktop.read_text())
        if shutil.which("desktop-file-validate"):
            subprocess.run(["desktop-file-validate", str(desktop)], check=True)
        self.assert_no_staging()

    def test_update_replaces_application_and_integrations_preserving_user_data(self):
        old_source = self.make_bundle("1.0")
        self.install(old_source)
        unrelated_icon = self.prefix / "share/icons/hicolor/scalable/apps/unrelated.svg"
        unrelated_icon.write_text("unrelated application")
        config = self.root / "config/tiny-player/settings.json"
        cache = self.root / "cache/tiny-player/server-icons/icon.img"
        for path in (config, cache):
            path.parent.mkdir(parents=True)
            path.write_bytes(b"existing user data")
        new_source = self.make_bundle("2.0")
        result = self.install(new_source, env=dict(
            os.environ, XDG_CONFIG_HOME=str(config.parents[1]), XDG_CACHE_HOME=str(cache.parents[2]),
        ))
        self.assertIn("Updated Tiny Player", result.stdout)
        application = self.prefix / "tiny-player.app"
        self.assertFalse((application / "lib/version-1.0.so").exists())
        self.assertEqual((application / "lib/version-2.0.so").read_text(), "2.0")
        self.assertEqual(subprocess.check_output([str(self.prefix / "bin/tiny-player")], text=True), "2.0\n")
        self.assertIn("Name=Tiny Player 2.0", (self.prefix / "share/applications/tiny-player.desktop").read_text())
        self.assertIn("<title>2.0</title>", (self.prefix / "share/icons/hicolor/scalable/apps/tiny-player.svg").read_text())
        self.assertEqual(unrelated_icon.read_text(), "unrelated application")
        for path in (config, cache):
            self.assertEqual(path.read_bytes(), b"existing user data")
        self.assertTrue((old_source / "lib/version-1.0.so").exists())
        self.assertTrue((new_source / "lib/version-2.0.so").exists())
        self.assert_no_staging()

    def test_update_removes_obsolete_icon_sizes_and_preserves_other_icons(self):
        self.install(self.make_bundle("1.0"))
        legacy = self.prefix / "share/icons/hicolor/512x512/apps/tiny-player.png"
        legacy.parent.mkdir(parents=True)
        legacy.write_bytes(b"legacy icon left by an earlier installer")
        unrelated = legacy.with_name("unrelated.png")
        unrelated.write_bytes(b"another application's icon")
        custom_icon = self.root / "custom.svg"
        custom_icon.write_text("custom icon")
        legacy_link = legacy.with_suffix(".svg")
        legacy_link.symlink_to(custom_icon)
        custom_theme_icon = self.prefix / "share/icons/custom/512x512/apps/tiny-player.png"
        custom_theme_icon.parent.mkdir(parents=True)
        custom_theme_icon.write_bytes(b"custom theme override")

        source = self.make_bundle("2.0")
        self.install(source)

        self.assertFalse(legacy.exists())
        self.assertFalse(legacy_link.is_symlink())
        self.assertEqual(custom_icon.read_text(), "custom icon")
        self.assertEqual(unrelated.read_bytes(), b"another application's icon")
        self.assertEqual(custom_theme_icon.read_bytes(), b"custom theme override")
        self.assertEqual(
            (self.prefix / "share/icons/hicolor/256x256/apps/tiny-player.png").read_bytes(),
            (source / "share/icons/hicolor/256x256/apps/tiny-player.png").read_bytes(),
        )
        self.assert_no_staging()

    def test_kde_icon_reload_runs_after_icons_and_desktop_entry_are_installed(self):
        source = self.make_bundle("1.0")
        log = self.root / "icon-refresh.log"
        env = self.command_environment("dbus-send", '''
[[ -f $TINY_PLAYER_TEST_PREFIX/share/applications/tiny-player.desktop ]] || exit 73
[[ -f $TINY_PLAYER_TEST_PREFIX/share/icons/hicolor/256x256/apps/tiny-player.png ]] || exit 73
printf '%s\\n' "$@" > "$TINY_PLAYER_TEST_REFRESH_LOG"
exit 0''')
        env.update(XDG_CURRENT_DESKTOP="KDE:PLASMA", TINY_PLAYER_TEST_PREFIX=str(self.prefix),
                   TINY_PLAYER_TEST_REFRESH_LOG=str(log))
        self.install(source, env=env)
        self.assertEqual(log.read_text().splitlines(), [
            "--session", "--type=signal", "/KIconLoader", "org.kde.KIconLoader.iconChanged", "int32:0",
        ])

    def test_other_desktops_do_not_send_kde_icon_reload(self):
        source = self.make_bundle("1.0")
        log = self.root / "icon-refresh.log"
        env = self.command_environment("dbus-send", '''
printf 'unexpected KDE refresh' > "$TINY_PLAYER_TEST_REFRESH_LOG"
exit 0''')
        env.update(XDG_CURRENT_DESKTOP="GNOME", TINY_PLAYER_TEST_REFRESH_LOG=str(log))
        self.install(source, env=env)
        self.assertFalse(log.exists())

    def test_failed_icon_reload_does_not_roll_back_a_successful_update(self):
        self.install(self.make_bundle("1.0"))
        env = self.command_environment("dbus-send", "exit 73")
        env["XDG_CURRENT_DESKTOP"] = "KDE"
        result = self.install(self.make_bundle("2.0"), env=env)
        self.assertIn("Updated Tiny Player", result.stdout)
        self.assertEqual(subprocess.check_output([str(self.prefix / "bin/tiny-player")], text=True), "2.0\n")
        self.assert_no_staging()

    def test_reinstallation_and_running_installed_script_are_idempotent(self):
        source = self.make_bundle("1.0")
        self.install(source)
        original = self.installed_files()
        self.install(source)
        self.assertEqual(self.installed_files(), original)
        alias = self.root / "prefix alias"
        alias.symlink_to(self.prefix, target_is_directory=True)
        result = self.install(alias / "tiny-player.app")
        self.assertIn("Already installed", result.stdout)
        self.assertEqual(self.installed_files(), original)
        self.assert_no_staging()

    def test_failed_copy_leaves_previous_installation_intact(self):
        self.install(self.make_bundle("1.0"))
        original = self.installed_files()
        result = self.install(self.make_bundle("2.0"), env=self.command_environment("cp", "exit 73"), check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.installed_files(), original)
        self.assert_no_staging()

    def test_incomplete_update_bundle_leaves_previous_installation_intact(self):
        self.install(self.make_bundle("1.0"))
        original = self.installed_files()
        source = self.make_bundle("2.0")
        (source / "share/applications/tiny-player.desktop").unlink()
        result = self.install(source, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Incomplete bundle", result.stderr)
        self.assertEqual(self.installed_files(), original)
        self.assert_no_staging()

    def test_failed_application_activation_restores_previous_installation(self):
        self.install(self.make_bundle("1.0"))
        original = self.installed_files()
        env = self.command_environment("mv", 'if [[ ${3:-} == */new-app ]]; then exit 73; fi')
        result = self.install(self.make_bundle("2.0"), env=env, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.installed_files(), original)
        self.assert_no_staging()

    def test_failed_or_interrupted_desktop_update_restores_all_replaced_files(self):
        self.install(self.make_bundle("1.0"))
        legacy = self.prefix / "share/icons/hicolor/512x512/apps/tiny-player.png"
        legacy.parent.mkdir(parents=True)
        legacy.write_bytes(b"legacy icon")
        legacy.with_suffix(".svg").symlink_to(self.root / "missing-icon.svg")
        original = self.installed_files()
        source = self.make_bundle("2.0")
        log = self.root / "icon-refresh.log"
        self.command_environment("dbus-send", '''
printf 'unexpected refresh after a failed update' > "$TINY_PLAYER_TEST_REFRESH_LOG"
exit 0''')
        for failure in ("exit 73", 'kill -TERM "$PPID"; exit 143'):
            with self.subTest(failure=failure):
                env = self.command_environment("mv", f'if [[ ${{3:-}} == */desktop-entry ]]; then {failure}; fi')
                env.update(XDG_CURRENT_DESKTOP="KDE", TINY_PLAYER_TEST_REFRESH_LOG=str(log))
                result = self.install(source, env=env, check=False)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.installed_files(), original)
                self.assertFalse(log.exists())
                self.assert_no_staging()

    def test_failed_first_install_removes_new_application_and_integrations(self):
        env = self.command_environment("mv", 'if [[ ${3:-} == */desktop-entry ]]; then exit 73; fi')
        result = self.install(self.make_bundle("1.0"), env=env, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.prefix / "tiny-player.app").exists())
        self.assertFalse((self.prefix / "bin/tiny-player").is_symlink())
        self.assertFalse((self.prefix / "share/icons/hicolor/scalable/apps/tiny-player.svg").exists())
        self.assertFalse((self.prefix / "share/applications/tiny-player.desktop").exists())
        self.assert_no_staging()

    def test_conflicting_desktop_directory_is_preserved_and_update_is_rolled_back(self):
        self.install(self.make_bundle("1.0"))
        desktop = self.prefix / "share/applications/tiny-player.desktop"
        desktop.unlink()
        desktop.mkdir()
        (desktop / "unrelated.txt").write_text("keep")
        original = self.installed_files()
        result = self.install(self.make_bundle("2.0"), check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Refusing to replace a directory", result.stderr)
        self.assertEqual(self.installed_files(), original)
        self.assert_no_staging()

    def test_failed_restore_keeps_previous_application_for_recovery(self):
        self.install(self.make_bundle("1.0"))
        env = self.command_environment("mv", '''
if [[ ${3:-} == */new-app || ( ${3:-} == */previous/* && ${4:-} == */tiny-player.app ) ]]; then
    exit 73
fi''')
        result = self.install(self.make_bundle("2.0"), env=env, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Recovery files remain", result.stderr)
        backups = list(self.prefix.glob(".tiny-player.installing.*/previous/*/bin/tiny-player"))
        self.assertEqual(len(backups), 1)
        self.assertEqual(subprocess.check_output([str(backups[0])], text=True), "1.0\n")

    def test_unrecognized_directory_and_symlink_destination_are_preserved(self):
        source = self.make_bundle("1.0")
        unrelated = self.root / "unrelated"
        unrelated.mkdir()
        (unrelated / "keep.txt").write_text("keep")
        for kind in ("directory", "symlink", "file"):
            with self.subTest(kind=kind):
                prefix = self.root / kind
                prefix.mkdir()
                destination = prefix / "tiny-player.app"
                if kind == "directory":
                    shutil.copytree(unrelated, destination)
                elif kind == "symlink":
                    destination.symlink_to(unrelated, target_is_directory=True)
                else:
                    destination.write_text("keep")
                self.write_test_installer(source, prefix)
                result = self.install(source, check=False)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("unrecognized installation", result.stderr)
                self.assertEqual((unrelated / "keep.txt").read_text(), "keep")
                if kind == "file":
                    self.assertEqual(destination.read_text(), "keep")
                else:
                    self.assertEqual((destination / "keep.txt").read_text(), "keep")

    def test_existing_standalone_launcher_is_preserved(self):
        source = self.make_bundle("1.0")
        launcher = self.prefix / "bin/tiny-player"
        launcher.parent.mkdir(parents=True)
        launcher.write_text("standalone executable")
        result = self.install(source, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(launcher.read_text(), "standalone executable")
        self.assertFalse((self.prefix / "tiny-player.app").exists())

    def test_source_containing_installation_directory_is_rejected_without_recursive_copy(self):
        source = self.make_bundle("1.0")
        self.write_test_installer(source, source / "nested prefix")
        result = self.install(source, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("outside the source bundle", result.stderr)
        self.assertFalse((source / "nested prefix/tiny-player.app").exists())

    def test_concurrent_install_is_rejected_without_changing_existing_files(self):
        source = self.make_bundle("1.0")
        self.install(source)
        original = self.installed_files()
        with (self.prefix / ".tiny-player.install.lock").open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = self.install(source, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("installation is in progress", result.stderr)
        self.assertEqual(self.installed_files(), original)
        self.assert_no_staging()


if __name__ == "__main__":
    unittest.main()
