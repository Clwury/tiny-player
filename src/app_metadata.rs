use std::path::PathBuf;

use anyhow::{Result, anyhow};
use directories::{BaseDirs, ProjectDirs};

pub(crate) const APP_ID: &str = "tiny-player";
pub(crate) const APP_NAME: &str = "Tiny Player";
pub(crate) const APP_ICON_ASSET_PATH: &str = "icons/tiny-player.png";

const PROJECT_QUALIFIER: &str = "dev";
const PROJECT_ORGANIZATION: &str = "tiny-player";

pub(crate) fn config_dir() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().to_path_buf())
}

pub(crate) fn cache_dir() -> Result<PathBuf> {
    Ok(default_playback_cache_dir())
}

pub(crate) fn default_playback_cache_dir() -> PathBuf {
    BaseDirs::new()
        .map(|dirs| dirs.home_dir().join(".cache"))
        .unwrap_or_else(|| PathBuf::from(".cache"))
        .join(APP_ID)
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
    fn caches_default_to_dot_cache_under_the_user_home() {
        let home = BaseDirs::new().expect("user home exists");
        let expected = home.home_dir().join(".cache/tiny-player");

        assert_eq!(cache_dir().unwrap(), expected);
        assert_eq!(default_playback_cache_dir(), expected);
    }

    #[test]
    fn desktop_entry_matches_application_metadata() {
        for expected in [
            format!("Name={APP_NAME}"),
            format!("Icon={APP_ID}"),
            format!("StartupWMClass={APP_ID}"),
            format!("Exec={APP_ID}"),
        ] {
            assert!(DESKTOP_ENTRY.lines().any(|line| line == expected));
        }
    }

    #[test]
    fn raster_icon_preserves_desktop_padding_and_rounded_corners() {
        let icon = image::load_from_memory(APP_ICON_BYTES)
            .unwrap()
            .into_rgba8();

        assert_eq!(icon.dimensions(), (256, 256));
        for (x, y, pixel) in icon.enumerate_pixels() {
            if !(16..240).contains(&x) || !(16..240).contains(&y) {
                assert_eq!(pixel.0[3], 0, "desktop padding at ({x}, {y})");
            }
        }
        for (x, y) in [(16, 16), (239, 16), (16, 239), (239, 239)] {
            assert_eq!(icon.get_pixel(x, y).0[3], 0);
        }
        for (x, y) in [(16, 128), (239, 128), (128, 16), (128, 239)] {
            assert_eq!(icon.get_pixel(x, y).0[3], 255);
        }
        assert_eq!(icon.get_pixel(128, 128).0[3], 255);
    }
}
