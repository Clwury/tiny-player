use crate::emby::MediaStream;

pub use tiny_playback::tracks::{PlaybackTrack, PlaybackTrackKind, PlaybackTrackSelection};

/// Application-side labels and Emby model conversion for engine tracks.
pub(crate) trait PlaybackTrackExt: Sized {
    fn from_audio_stream(stream: &MediaStream, index: usize) -> Option<Self>;
    fn from_subtitle_stream(stream: &MediaStream, index: usize) -> Option<Self>;
    fn metadata_label(&self) -> String;
}

impl PlaybackTrackExt for PlaybackTrack {
    fn from_audio_stream(stream: &MediaStream, index: usize) -> Option<Self> {
        Some(with_stream_metadata(
            Self::new(
                usize::try_from(stream.index?).ok()?,
                stream.audio_label(index),
                stream.is_external.unwrap_or(false),
            ),
            stream,
        ))
    }

    fn from_subtitle_stream(stream: &MediaStream, index: usize) -> Option<Self> {
        Some(with_stream_metadata(
            Self::new(
                usize::try_from(stream.index?).ok()?,
                stream.display_title_label(index),
                stream.is_external.unwrap_or(false),
            ),
            stream,
        ))
    }

    fn metadata_label(&self) -> String {
        super::track_metadata_label(self.language.as_deref(), self.title.as_deref())
    }
}

fn with_stream_metadata(mut track: PlaybackTrack, stream: &MediaStream) -> PlaybackTrack {
    track.language = stream.language.clone();
    track.title = stream.title.clone();
    track.with_codec(stream.codec.clone())
}
