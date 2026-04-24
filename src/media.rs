use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const SUPPORTED_EXTENSIONS: &[&str] = &["mp4", "mkv", "webm", "mov"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistEntry {
    pub path: PathBuf,
    pub display_name: String,
}

pub fn discover_playlist(sample_dir: &Path) -> Result<Vec<PlaylistEntry>> {
    let mut entries = Vec::new();

    for item in fs::read_dir(sample_dir)
        .with_context(|| format!("failed to read {}", sample_dir.display()))?
    {
        let item = item?;
        let path = item.path();
        if !path.is_file() {
            continue;
        }

        let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
            continue;
        };

        if !SUPPORTED_EXTENSIONS
            .iter()
            .any(|supported| extension.eq_ignore_ascii_case(supported))
        {
            continue;
        }

        let display_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| path.display().to_string());

        entries.push(PlaylistEntry { path, display_name });
    }

    entries.sort_by(|left, right| left.display_name.cmp(&right.display_name));
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn discover_playlist_filters_and_sorts() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("b.webm"), b"").unwrap();
        fs::write(dir.path().join("a.mp4"), b"").unwrap();
        fs::write(dir.path().join("ignore.txt"), b"").unwrap();

        let playlist = discover_playlist(dir.path()).unwrap();
        let names: Vec<_> = playlist
            .iter()
            .map(|entry| entry.display_name.as_str())
            .collect();

        assert_eq!(names, vec!["a.mp4", "b.webm"]);
    }

    #[test]
    fn discover_playlist_returns_empty_for_empty_dir() {
        let dir = tempdir().unwrap();
        let playlist = discover_playlist(dir.path()).unwrap();

        assert!(playlist.is_empty());
    }
}
