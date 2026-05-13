use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let library = pkg_config::Config::new()
        .atleast_version("7")
        .probe("libplacebo")
        .expect("libplacebo 7 is required for HDR tone mapping");
    let mut builder = bindgen::Builder::default()
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

    for include_path in library.include_paths {
        builder = builder.clang_arg(format!("-I{}", include_path.display()));
    }

    let bindings = builder
        .generate()
        .expect("failed to generate libplacebo bindings");
    let out_path = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set"));
    bindings
        .write_to_file(out_path.join("libplacebo_bindings.rs"))
        .expect("failed to write libplacebo bindings");

    let ffmpeg_vulkan_bindings = bindgen::Builder::default()
        .header_contents(
            "ffmpeg_vulkan_wrapper.h",
            "#include <libavutil/hwcontext_vulkan.h>\n",
        )
        .allowlist_type("AVVkFrame")
        .allowlist_type("AVVulkan.*")
        .allowlist_type("Vk.*")
        .generate_comments(false)
        .derive_debug(false)
        .layout_tests(false)
        .generate()
        .expect("failed to generate FFmpeg Vulkan bindings");

    ffmpeg_vulkan_bindings
        .write_to_file(out_path.join("ffmpeg_vulkan_bindings.rs"))
        .expect("failed to write FFmpeg Vulkan bindings");
}
