use std::{
    borrow::Cow,
    fs,
    path::{Path, PathBuf},
};

use anyhow::Result;
use gpui::{AssetSource, SharedString};

pub struct ProjectAssets {
    base: PathBuf,
}

impl ProjectAssets {
    pub fn new() -> Self {
        Self::from_roots(
            option_env!("TINY_ASSET_DIR"),
            std::env::current_exe().ok().as_deref(),
            env!("CARGO_MANIFEST_DIR"),
        )
    }

    fn from_roots(
        installed_asset_dir: Option<&str>,
        executable: Option<&Path>,
        manifest_dir: &str,
    ) -> Self {
        let base = installed_asset_dir
            .map(PathBuf::from)
            .or_else(|| {
                // current_exe resolves the ~/.local/bin symlink on Linux, so
                // this also works after installing or moving a portable bundle.
                executable?
                    .parent()?
                    .parent()
                    .map(|root| root.join("share/tiny-player/assets"))
                    .filter(|assets| assets.is_dir())
            })
            .unwrap_or_else(|| PathBuf::from(manifest_dir).join("assets"));
        Self { base }
    }
}

impl AssetSource for ProjectAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        fs::read(self.base.join(path))
            .map(|data| Some(Cow::Owned(data)))
            .map_err(Into::into)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        fs::read_dir(self.base.join(path))
            .map(|entries| {
                entries
                    .filter_map(|entry| {
                        entry
                            .ok()
                            .and_then(|entry| entry.file_name().into_string().ok())
                            .map(SharedString::from)
                    })
                    .collect()
            })
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_build_uses_manifest_assets() {
        let assets = ProjectAssets::from_roots(None, None, "/checkout/tiny-player");

        assert_eq!(assets.base, PathBuf::from("/checkout/tiny-player/assets"));
    }

    #[test]
    fn packaged_build_uses_installed_asset_directory() {
        let assets = ProjectAssets::from_roots(
            Some("/usr/share/tiny-player/assets"),
            None,
            "/build/tiny-player",
        );

        assert_eq!(assets.base, PathBuf::from("/usr/share/tiny-player/assets"));
    }

    #[test]
    fn portable_assets_load_after_moving_the_bundle() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        let moved = temp.path().join("moved bundle");
        let icons = original.join("share/tiny-player/assets/icons");
        fs::create_dir_all(&icons).unwrap();
        fs::write(icons.join("play.svg"), b"<svg/>").unwrap();
        fs::rename(&original, &moved).unwrap();

        let assets = ProjectAssets::from_roots(
            None,
            Some(&moved.join("bin/tiny-player")),
            "/unavailable/build/checkout",
        );

        assert_eq!(
            assets.load("icons/play.svg").unwrap().unwrap().as_ref(),
            b"<svg/>"
        );
        assert_eq!(
            assets.list("icons").unwrap(),
            vec![SharedString::from("play.svg")]
        );
    }

    #[test]
    fn development_binary_without_bundle_assets_uses_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let assets = ProjectAssets::from_roots(
            None,
            Some(&temp.path().join("target/debug/tiny-player")),
            "/checkout/tiny-player",
        );

        assert_eq!(assets.base, PathBuf::from("/checkout/tiny-player/assets"));
    }
}
