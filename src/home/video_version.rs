use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::emby::MediaSource;
use crate::player::SavedTrackChoices;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct VideoVersion {
    pub(crate) source_id: String,
    pub(crate) name: Option<String>,
    /// Track choices belong to each source, independently of the last played version.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(crate) track_preferences: HashMap<String, SavedTrackChoices>,
}

impl VideoVersion {
    pub(crate) fn from_source(source: &MediaSource) -> Self {
        Self {
            source_id: source.id.clone().unwrap_or_default(),
            name: source.name.clone(),
            track_preferences: HashMap::new(),
        }
    }

    pub(crate) fn find_source(&self, sources: &[MediaSource]) -> Option<usize> {
        if !self.source_id.is_empty()
            && let Some(index) = sources
                .iter()
                .position(|source| source.id.as_deref() == Some(self.source_id.as_str()))
        {
            return Some(index);
        }
        let name = self.name.as_deref().filter(|name| !name.is_empty())?;
        // Tsukimi retains a version name and picks its closest Jaro-Winkler
        // match when the server rebuilds the media-source list.
        let mut best = None;
        let mut best_score = 0.0;
        for (index, source) in sources.iter().enumerate() {
            let Some(candidate) = source.name.as_deref() else {
                continue;
            };
            let score = strsim::jaro_winkler(candidate, name);
            if score > best_score {
                best_score = score;
                best = Some(index);
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_matcher_retains_quality_when_server_source_ids_change() {
        let version = VideoVersion {
            source_id: "old-source".into(),
            name: Some("S01E07.2160p.BDRip.H.265.FLAC".into()),
            ..Default::default()
        };
        let sources: Vec<MediaSource> = serde_json::from_value(serde_json::json!([
            {"Id": "1080", "Name": "S01E07 - 1080p"},
            {"Id": "2160", "Name": "(1998) - S01E07.2160p.BDRip.H.265.FLAC"}
        ]))
        .unwrap();
        assert_eq!(version.find_source(&sources), Some(1));
        assert_eq!(version.find_source(&[]), None);
    }

    #[test]
    fn exact_source_id_wins_when_version_names_are_identical() {
        let sources: Vec<MediaSource> = serde_json::from_value(serde_json::json!([
            {"Id": "first", "Name": "Same label"},
            {"Id": "played", "Name": "Same label"}
        ]))
        .unwrap();
        let version = VideoVersion {
            source_id: "played".into(),
            name: Some("Same label".into()),
            ..Default::default()
        };
        assert_eq!(version.find_source(&sources), Some(1));
    }
}
