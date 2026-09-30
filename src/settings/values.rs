use crate::app_metadata::default_playback_cache_dir;
use std::path::{Path, PathBuf};
pub(crate) const BYTES_PER_MIB: u64 = 1024 * 1024;

pub(crate) fn bytes_to_mib(bytes: u64) -> u64 {
    bytes / BYTES_PER_MIB
}

pub(crate) fn parse_seconds(value: &str) -> Option<f64> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
}

pub(crate) fn format_seconds(value: f64) -> String {
    let value = if value.is_finite() && value >= 0.0 {
        value
    } else {
        0.0
    };
    let mut formatted = format!("{value:.3}");
    while formatted.ends_with('0') {
        formatted.pop();
    }
    if formatted.ends_with('.') {
        formatted.pop();
    }
    if formatted.is_empty() {
        "0".to_string()
    } else {
        formatted
    }
}

pub(crate) fn resolved_cache_directories(
    configured_dir: Option<&Path>,
    environment_dirs: [Option<PathBuf>; 2],
) -> [PathBuf; 2] {
    // Match the HTTP and demux disk caches: configuration, per-cache override,
    // then the application's temporary directory. Only resolve for display.
    environment_dirs.map(|directory| {
        configured_dir
            .map(Path::to_path_buf)
            .or(directory)
            .unwrap_or_else(default_playback_cache_dir)
    })
}

pub(crate) fn matches_search(query: &str, fields: &[&str]) -> bool {
    let haystack = fields.join(" ").to_lowercase();
    query
        .split_whitespace()
        .all(|word| haystack.contains(&word.to_lowercase()))
}
