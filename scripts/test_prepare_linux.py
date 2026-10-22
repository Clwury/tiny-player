import importlib.util
from pathlib import Path
import tempfile
import tomllib
import unittest


SPEC = importlib.util.spec_from_file_location("prepare_linux", Path(__file__).with_name("prepare-linux.py"))
prepare = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(prepare)


class LinuxPreparationTests(unittest.TestCase):
    def test_cargo_uses_matching_headers_and_runtime_with_existing_search_paths(self):
        prefix = Path('/tmp/Linux development "测试🎬"/prefix')
        config = tomllib.loads(prepare.cargo_config(prefix, {
            "PKG_CONFIG_PATH": "/other/lib/pkgconfig",
            "LD_LIBRARY_PATH": "/other/lib",
        }))
        environment = config["env"]
        self.assertEqual(environment["PKG_CONFIG_PATH"], {
            "value": f"{prefix}/lib/pkgconfig:/other/lib/pkgconfig", "force": True,
        })
        self.assertEqual(environment["LD_LIBRARY_PATH"], {
            "value": f"{prefix}/lib:/other/lib", "force": True,
        })
        self.assertEqual(environment["TINY_LIBPLACEBO_LIB_DIR"], {
            "value": f"{prefix}/lib", "force": True,
        })

    def test_preparation_preserves_unrelated_cargo_configuration(self):
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "config.toml"
            original = '[build]\ntarget = "aarch64-unknown-linux-gnu"\n'
            config.write_text(original)
            with self.assertRaisesRegex(RuntimeError, "Preserving existing"):
                prepare.check_config_owner(config)
            self.assertEqual(config.read_text(), original)

    def test_api_version_without_queue_flags_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix = Path(directory)
            include = prefix / "include/libplacebo"
            include.mkdir(parents=True)
            (include / "config.h").write_text("#define PL_API_VER 365\n")
            (include / "vulkan.h").write_text("struct pl_vulkan_queue { unsigned index; unsigned count; };\n")
            with self.assertRaisesRegex(RuntimeError, "queue creation flags"):
                prepare.validate_headers(prefix)
            (include / "vulkan.h").write_text(
                "struct pl_vulkan_queue { unsigned index; unsigned count; unsigned flags; };\n",
            )
            prepare.validate_headers(prefix)

    def test_missing_runtime_does_not_reuse_a_completed_build_stamp(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix = Path(directory)
            include = prefix / "include/libplacebo"
            include.mkdir(parents=True)
            (prefix / "libplacebo.commit").write_text("pinned-commit\n")
            (include / "config.h").write_text("#define PL_API_VER 365\n")
            (include / "vulkan.h").write_text("struct pl_vulkan_queue { unsigned flags; };\n")
            self.assertFalse(prepare.prepared(prefix, "pinned-commit"))


if __name__ == "__main__":
    unittest.main()
