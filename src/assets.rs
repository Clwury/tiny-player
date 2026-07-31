use std::{borrow::Cow, fs, path::PathBuf};

use anyhow::Result;
use gpui::{AssetSource, SharedString};

pub struct ProjectAssets {
    base: PathBuf,
}

impl ProjectAssets {
    pub fn new() -> Self {
        Self::from_roots(option_env!("TINY_ASSET_DIR"), env!("CARGO_MANIFEST_DIR"))
    }

    fn from_roots(installed_asset_dir: Option<&str>, manifest_dir: &str) -> Self {
        let base = installed_asset_dir
            .map_or_else(|| PathBuf::from(manifest_dir).join("assets"), PathBuf::from);
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
        let assets = ProjectAssets::from_roots(None, "/checkout/tiny-player");

        assert_eq!(assets.base, PathBuf::from("/checkout/tiny-player/assets"));
    }

    #[test]
    fn packaged_build_uses_installed_asset_directory() {
        let assets =
            ProjectAssets::from_roots(Some("/usr/share/tiny-player/assets"), "/build/tiny-player");

        assert_eq!(assets.base, PathBuf::from("/usr/share/tiny-player/assets"));
    }
}
