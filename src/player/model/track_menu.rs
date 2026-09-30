//! Snapshot of the open track menu. Intents retain exactly the track selected
//! when the menu was drawn, including external subtitle URL/codec metadata.
use crate::player::PlaybackTrackExt;
use tiny_playback::{PlaybackTrack, PlaybackTrackKind};

pub(in crate::player) enum TrackMenuIntent {
    Audio(Option<usize>),
    Subtitle(Option<PlaybackTrack>),
}

pub(in crate::player) struct TrackMenuOptionVm {
    pub(in crate::player) label: String,
    pub(in crate::player) metadata: String,
    pub(in crate::player) selected: bool,
    pub(in crate::player) intent: TrackMenuIntent,
}

pub(in crate::player) struct TrackMenuVm {
    pub(in crate::player) id: &'static str,
    pub(in crate::player) off_selected: bool,
    pub(in crate::player) off_intent: TrackMenuIntent,
    pub(in crate::player) options: Vec<TrackMenuOptionVm>,
}

pub(in crate::player) fn track_menu(
    kind: PlaybackTrackKind,
    tracks: &[PlaybackTrack],
    selected: Option<usize>,
) -> TrackMenuVm {
    let (id, off_intent) = match kind {
        PlaybackTrackKind::Audio => ("playback-audio-menu", TrackMenuIntent::Audio(None)),
        PlaybackTrackKind::Subtitle => ("playback-caption-menu", TrackMenuIntent::Subtitle(None)),
    };
    TrackMenuVm {
        id,
        off_selected: selected.is_none(),
        off_intent,
        options: tracks
            .iter()
            .map(|track| TrackMenuOptionVm {
                label: if track.is_external {
                    format!("{} 外挂", track.label)
                } else {
                    track.label.clone()
                },
                metadata: track.metadata_label(),
                selected: selected == Some(track.stream_index),
                intent: match kind {
                    PlaybackTrackKind::Audio => TrackMenuIntent::Audio(Some(track.stream_index)),
                    PlaybackTrackKind::Subtitle => TrackMenuIntent::Subtitle(Some(track.clone())),
                },
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_preserves_stream_ids_external_metadata_and_missing_selection() {
        let tracks = [
            PlaybackTrack::new(7, "内嵌", false),
            PlaybackTrack::new(19, "中文字幕", true)
                .with_external_url(Some("file:///subtitle.ass".into()))
                .with_codec(Some("ass".into())),
        ];
        let subtitles = track_menu(PlaybackTrackKind::Subtitle, &tracks, Some(19));
        assert_eq!(subtitles.id, "playback-caption-menu");
        assert!(!subtitles.off_selected);
        assert!(!subtitles.options[0].selected);
        assert!(subtitles.options[1].selected);
        assert_eq!(subtitles.options[1].label, "中文字幕 外挂");
        assert_eq!(subtitles.options[1].metadata, tracks[1].metadata_label());
        let TrackMenuIntent::Subtitle(Some(track)) = &subtitles.options[1].intent else {
            panic!("subtitle choice")
        };
        assert_eq!(track, &tracks[1]);
        assert!(matches!(
            subtitles.off_intent,
            TrackMenuIntent::Subtitle(None)
        ));
        let audio = track_menu(PlaybackTrackKind::Audio, &tracks, None);
        assert_eq!(audio.id, "playback-audio-menu");
        assert!(audio.off_selected);
        assert!(matches!(audio.off_intent, TrackMenuIntent::Audio(None)));
        assert!(matches!(
            audio.options[0].intent,
            TrackMenuIntent::Audio(Some(7))
        ));
        let missing = track_menu(PlaybackTrackKind::Audio, &tracks, Some(999));
        assert!(!missing.off_selected);
        assert!(missing.options.iter().all(|option| !option.selected));
    }
}
