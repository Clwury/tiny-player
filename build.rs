use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_LIBDIR");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_SYSROOT_DIR");

    let libplacebo = pkg_config::Config::new()
        .atleast_version("7")
        .probe("libplacebo")
        .expect("libplacebo 7 is required for HDR tone mapping");
    let builder = bindgen::Builder::default()
        .header_contents(
            "libplacebo_wrapper.h",
            "#include <libplacebo/vulkan.h>\n#include <libplacebo/renderer.h>\n#include <libplacebo/utils/upload.h>\n#include <libplacebo/utils/dolbyvision.h>\n",
        )
        .allowlist_function("pl_.*")
        .allowlist_type("pl_.*")
        .allowlist_var("pl_.*")
        .allowlist_var("PL_.*")
        .generate_comments(false)
        .derive_debug(false)
        .layout_tests(false);
    let bindings = add_pkg_config_include_args(builder, &libplacebo)
        .generate()
        .expect("failed to generate libplacebo bindings");
    let out_path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set"));
    write_bindings_file(&out_path, "libplacebo_bindings.rs", &bindings);

    let libavutil = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libavutil")
        .expect("libavutil pkg-config metadata is required to generate FFmpeg Vulkan bindings");
    let ffmpeg_vulkan_builder = bindgen::Builder::default()
        .header_contents(
            "ffmpeg_vulkan_wrapper.h",
            "#include <libavutil/hwcontext_vulkan.h>\n",
        )
        .allowlist_type("AVVkFrame")
        .allowlist_type("AVVulkan.*")
        .allowlist_type("Vk.*")
        .generate_comments(false)
        .derive_debug(false)
        .layout_tests(false);
    let ffmpeg_vulkan_bindings = add_pkg_config_include_args(ffmpeg_vulkan_builder, &libavutil)
        .generate()
        .expect("failed to generate FFmpeg Vulkan bindings");
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
