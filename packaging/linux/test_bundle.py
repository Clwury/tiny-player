from pathlib import Path
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
    def test_install_with_spaces_preserves_original_and_refuses_overwrite(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source bundle"
            (source / "bin").mkdir(parents=True)
            (source / "share/applications").mkdir(parents=True)
            (source / "share/icons/hicolor").mkdir(parents=True)
            executable = source / "bin/tiny-player"
            executable.write_text("#!/bin/sh\nexit 0\n")
            executable.chmod(0o755)
            (source / "share/applications/tiny-player.desktop").write_text(
                "[Desktop Entry]\nType=Application\nName=Tiny Player\nExec=tiny-player\n"
            )
            script = source / "install.sh"
            shutil.copy2(Path(__file__).with_name("install.sh"), script)
            prefix = root / "install with spaces"
            subprocess.run(["bash", str(script), str(prefix)], check=True, capture_output=True)
            link = prefix / "bin/tiny-player"
            self.assertEqual(link.resolve(), prefix / "tiny-player.app/bin/tiny-player")
            subprocess.run([str(link)], check=True)
            self.assertTrue(executable.exists())
            desktop = prefix / "share/applications/tiny-player.desktop"
            self.assertIn(f'Exec="{link}"', desktop.read_text())
            if shutil.which("desktop-file-validate"):
                subprocess.run(["desktop-file-validate", str(desktop)], check=True)
            result = subprocess.run(["bash", str(script), str(prefix)], capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertTrue(link.resolve().exists())


if __name__ == "__main__":
    unittest.main()
