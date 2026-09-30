//! Authenticated subtitle URL construction at the Emby adapter boundary.
//! This assembles descriptors only; IO stays in the gateway/engine.
use crate::player::PlaybackTrackExt;
use tiny_playback::PlaybackTrack;

pub(crate) fn playback_subtitle_tracks_for_source(
    source: &crate::emby::MediaSource,
    server: &crate::server::CachedServer,
    item_id: &str,
    media_source_id: &str,
) -> Vec<PlaybackTrack> {
    source
        .subtitle_streams()
        .into_iter()
        .enumerate()
        .filter_map(|(index, stream)| {
            let external_url =
                playback_subtitle_external_url(stream, server, item_id, media_source_id);
            Some(
                PlaybackTrack::from_subtitle_stream(stream, index)?.with_external_url(external_url),
            )
        })
        .collect()
}

fn playback_subtitle_external_url(
    stream: &crate::emby::MediaStream,
    server: &crate::server::CachedServer,
    item_id: &str,
    media_source_id: &str,
) -> Option<String> {
    if !is_external_subtitle_stream(stream) {
        return None;
    }

    let delivery_url = stream
        .delivery_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| fallback_external_subtitle_delivery_url(stream, item_id, media_source_id))?;
    let mut url = crate::emby::playback::resolve_direct_stream_url(server, &delivery_url).ok()?;
    if !url.query_pairs().any(|(name, _)| name == "api_key")
        && let Some(access_token) = server
            .access_token
            .as_deref()
            .filter(|token| !token.is_empty())
    {
        url.query_pairs_mut().append_pair("api_key", access_token);
    }
    Some(url.to_string())
}

fn is_external_subtitle_stream(stream: &crate::emby::MediaStream) -> bool {
    stream.is_external.unwrap_or(false)
        || stream
            .delivery_method
            .as_deref()
            .is_some_and(|method| method.eq_ignore_ascii_case("External"))
}

fn fallback_external_subtitle_delivery_url(
    stream: &crate::emby::MediaStream,
    item_id: &str,
    media_source_id: &str,
) -> Option<String> {
    let stream_index = stream.index?;
    let extension = external_subtitle_extension(stream.codec.as_deref())?;
    Some(format!(
        "/Videos/{item_id}/{media_source_id}/Subtitles/{stream_index}/0/Stream.{extension}"
    ))
}

fn external_subtitle_extension(codec: Option<&str>) -> Option<&'static str> {
    match codec?.trim().to_ascii_lowercase().as_str() {
        "ass" => Some("ass"),
        "ssa" => Some("ssa"),
        "subrip" | "srt" => Some("srt"),
        "vtt" | "webvtt" => Some("vtt"),
        "sub" => Some("sub"),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
