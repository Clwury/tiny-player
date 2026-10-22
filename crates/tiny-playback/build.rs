use std::{
    cell::Cell,
    env, fs,
    path::{Path, PathBuf},
    rc::Rc,
};

const LIBPLACEBO_MIN_VERSION: &str = "7";
const LIBPLACEBO_MAX_VERSION: &str = "8";
const LIBAVUTIL_MIN_VERSION: &str = "60.26.100";
// FFmpeg 9 uses libavutil 61.x. Keep the upper bound at the next major
// version so FFmpeg 8.1 and 9 installations are accepted while an ABI
// breaking FFmpeg 10 (libavutil 62) is still rejected explicitly.
const LIBAVUTIL_MAX_VERSION: &str = "62";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_LIBDIR");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_SYSROOT_DIR");
    println!("cargo:rerun-if-env-changed=TINY_LIBPLACEBO_LIB_DIR");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
        && let Some(directory) = env::var_os("TINY_LIBPLACEBO_LIB_DIR")
    {
        let directory = PathBuf::from(directory);
        // Engine tests and examples also need the prepared runtime library.
        println!("cargo:rustc-link-search=native={}", directory.display());
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", directory.display());
    }

    let libplacebo = pkg_config::Config::new()
        .range_version(LIBPLACEBO_MIN_VERSION..LIBPLACEBO_MAX_VERSION)
        .probe("libplacebo")
        .expect(
            "supported libplacebo range is >= 7 and < 8; install libplacebo 7.x for HDR tone mapping",
        );
    let placebo_queue_flags = StructFieldProbe::new("pl_vulkan_queue", "flags");
    let builder = bindgen::Builder::default()
        .header_contents(
            "libplacebo_wrapper.h",
            "#include <libplacebo/vulkan.h>\n#include <libplacebo/renderer.h>\n#include <libplacebo/utils/upload.h>\n#include <libplacebo/utils/dolbyvision.h>\n",
        )
        .allowlist_function("pl_.*")
        .allowlist_type("pl_.*")
        .allowlist_var("pl_.*")
        .allowlist_var("PL_.*")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .parse_callbacks(Box::new(placebo_queue_flags.clone()))
        .generate_comments(false)
        .derive_debug(false)
        .layout_tests(false);
    let bindings = add_pkg_config_include_args(builder, &libplacebo)
        .generate()
        .expect("failed to generate libplacebo bindings");
    placebo_queue_flags.emit_cfg("libplacebo_vulkan_queue_flags");
    let out_path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set"));
    write_bindings_file(&out_path, "libplacebo_bindings.rs", &bindings);

    let libavutil = pkg_config::Config::new()
        .cargo_metadata(false)
        .range_version(LIBAVUTIL_MIN_VERSION..LIBAVUTIL_MAX_VERSION)
        .probe("libavutil")
        .expect(
            "supported libavutil range is >= 60.26.100 and < 62 (FFmpeg >= 8.1 and < 10); install FFmpeg 8.1+ or 9.x",
        );
    let ffmpeg_queue_flags = StructFieldProbe::new("AVVulkanDeviceContext", "queue_flags");
    let ffmpeg_vulkan_builder = bindgen::Builder::default()
        .header_contents(
            "ffmpeg_vulkan_wrapper.h",
            "#include <libavutil/hwcontext_vulkan.h>\n#include <libavutil/hwcontext_drm.h>\n",
        )
        .allowlist_type("AVVkFrame")
        .allowlist_type("AVVulkan.*")
        .allowlist_type("AVDRM.*")
        .allowlist_type("Vk.*")
        .allowlist_type("PFN_vkGetPhysicalDeviceProperties2")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .parse_callbacks(Box::new(ffmpeg_queue_flags.clone()))
        .generate_comments(false)
        .derive_debug(false)
        .layout_tests(false);
    let ffmpeg_vulkan_bindings = add_pkg_config_include_args(ffmpeg_vulkan_builder, &libavutil)
        .generate()
        .expect("failed to generate FFmpeg Vulkan bindings");
    ffmpeg_queue_flags.emit_cfg("ffmpeg_vulkan_queue_flags");
    let ffmpeg_vulkan_text = bindings_text(&ffmpeg_vulkan_bindings, "FFmpeg Vulkan");
    assert_generated_types(
        &ffmpeg_vulkan_text,
        "FFmpeg Vulkan",
        &[
            "AVHWDeviceContext",
            "AVHWFramesContext",
            "AVVkFrame",
            "AVVulkanDeviceContext",
            "AVVulkanFramesContext",
        ],
    );
    fs::write(
        out_path.join("ffmpeg_vulkan_bindings.rs"),
        ffmpeg_vulkan_text,
    )
    .expect("failed to write FFmpeg Vulkan bindings");
}

// Probe the generated fields rather than package versions: distributions may
// backport Vulkan interop changes without changing the upstream version.
#[derive(Clone, Debug)]
struct StructFieldProbe {
    type_name: &'static str,
    field_name: &'static str,
    present: Rc<Cell<bool>>,
}

impl StructFieldProbe {
    fn new(type_name: &'static str, field_name: &'static str) -> Self {
        Self {
            type_name,
            field_name,
            present: Rc::new(Cell::new(false)),
        }
    }

    fn emit_cfg(&self, name: &str) {
        println!("cargo:rustc-check-cfg=cfg({name})");
        if self.present.get() {
            println!("cargo:rustc-cfg={name}");
        }
    }
}

impl bindgen::callbacks::ParseCallbacks for StructFieldProbe {
    fn field_visibility(
        &self,
        info: bindgen::callbacks::FieldInfo<'_>,
    ) -> Option<bindgen::FieldVisibilityKind> {
        if info.type_name == self.type_name && info.field_name == self.field_name {
            self.present.set(true);
        }
        None
    }
}

fn add_pkg_config_include_args(
    mut builder: bindgen::Builder,
    library: &pkg_config::Library,
) -> bindgen::Builder {
    for include_path in &library.include_paths {
        builder = builder.clang_arg(format!("-I{}", include_path.display()));
    }
    builder
}

fn write_bindings_file(out_path: &Path, file_name: &str, bindings: &bindgen::Bindings) {
    let text = bindings_text(bindings, file_name);
    // MSVC needs dllimport semantics for exported data as well as functions.
    // Cargo's link search/library directives alone do not attach that information
    // to bindgen's extern blocks (e.g. pl_render_default_params).
    let text = text.replace(
        "unsafe extern \"C\" {",
        "#[cfg_attr(windows, link(name = \"placebo\", kind = \"dylib\"))]\nunsafe extern \"C\" {",
    );
    fs::write(out_path.join(file_name), text).expect("failed to write generated bindings");
}

fn bindings_text(bindings: &bindgen::Bindings, description: &str) -> String {
    let mut output = Vec::new();
    bindings
        .write(Box::new(&mut output))
        .unwrap_or_else(|error| panic!("failed to serialize {description} bindings: {error}"));
    String::from_utf8(output)
        .unwrap_or_else(|error| panic!("{description} bindings were not valid UTF-8: {error}"))
}

fn assert_generated_types(bindings: &str, description: &str, types: &[&str]) {
    for ty in types {
        let struct_needle = format!("pub struct {ty}");
        let alias_needle = format!("pub type {ty}");
        if !bindings.contains(&struct_needle) && !bindings.contains(&alias_needle) {
            panic!("{description} bindings are missing required type `{ty}`");
        }
    }
}
