use std::path::PathBuf;

use anyhow::{Result, anyhow};
use directories::ProjectDirs;

pub(crate) const APP_ID: &str = "tiny-player";
pub(crate) const APP_NAME: &str = "Tiny Player";
pub(crate) const APP_ICON_ASSET_PATH: &str = "icons/tiny-player.png";

const PROJECT_QUALIFIER: &str = "dev";
const PROJECT_ORGANIZATION: &str = "tiny-player";

pub(crate) fn config_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().to_path_buf())
}

pub(crate) fn cache_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.cache_dir().to_path_buf())
}

fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from(PROJECT_QUALIFIER, PROJECT_ORGANIZATION, APP_NAME)
        .ok_or_else(|| anyhow!("无法定位 Tiny Player 用户目录"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP_ICON_BYTES: &[u8] = include_bytes!("../assets/icons/tiny-player.png");
    const DESKTOP_ENTRY: &str = include_str!("../tiny-player.desktop");

    #[test]
    fn desktop_entry_matches_application_metadata() {
        assert!(DESKTOP_ENTRY.contains(&format!("Name={APP_NAME}\n")));
        assert!(DESKTOP_ENTRY.contains(&format!("Icon={APP_ID}\n")));
        assert!(DESKTOP_ENTRY.contains(&format!("StartupWMClass={APP_ID}\n")));
        assert!(DESKTOP_ENTRY.contains(&format!("Exec={APP_ID}\n")));
    }

    #[test]
    fn raster_icon_preserves_transparent_rounded_corners() {
        let icon = image::load_from_memory(APP_ICON_BYTES)
            .unwrap()
            .into_rgba8();

        assert_eq!(icon.dimensions(), (512, 512));
        assert_eq!(icon.get_pixel(0, 0).0[3], 0);
        assert_eq!(icon.get_pixel(511, 0).0[3], 0);
        assert_eq!(icon.get_pixel(0, 511).0[3], 0);
        assert_eq!(icon.get_pixel(511, 511).0[3], 0);
        assert_eq!(icon.get_pixel(256, 256).0[3], 255);
    }
}
