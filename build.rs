#[cfg(windows)]
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=TINY_ASSET_DIR");
    #[cfg(windows)]
    embed_windows_resources();
}

#[cfg(windows)]
fn embed_windows_resources() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=packaging/windows/app.ico");
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let major = env::var("CARGO_PKG_VERSION_MAJOR").unwrap();
    let minor = env::var("CARGO_PKG_VERSION_MINOR").unwrap();
    let patch = env::var("CARGO_PKG_VERSION_PATCH").unwrap();
    let icon = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("packaging/windows/app.ico")
        .display()
        .to_string()
        .replace('\\', "/");
    let resource = PathBuf::from(env::var("OUT_DIR").unwrap()).join("tiny-player.rc");
    fs::write(
        &resource,
        format!(
            r#"
1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "FileDescription", "Tiny Player"
            VALUE "FileVersion", "{version}"
            VALUE "ProductName", "Tiny Player"
            VALUE "ProductVersion", "{version}"
            VALUE "OriginalFilename", "tiny-player.exe"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 1200
    END
END
"#
        ),
    )
    .expect("failed to write Windows resource file");
    // GPUI supplies the DPI/UAC manifest; only add our icon and version here.
    embed_resource::compile_for(resource, ["tiny-player"], embed_resource::NONE)
        .manifest_required()
        .expect("failed to compile Windows resources");
}
