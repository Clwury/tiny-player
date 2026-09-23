use std::{
    ffi::CStr,
    os::raw::c_int,
    sync::{Arc, mpsc::Sender},
};

use ffmpeg_sys_next as ffi;

use crate::{
    PlaybackTrack,
    backend::{BackendEvent, BackendEventKind, BackendSubtitleCue},
    render_host::RenderSize,
};

use super::super::avio::{CachedInputSource, should_cache_http_url};
use super::{
    Decoder, FfmpegControl, FfmpegPlaybackInput, FormatContext, HardwareDecodeMode,
    InputProbeProfile, StreamInfo, load_external_subtitle_cues,
};

pub(super) struct OpenedPlaybackInput {
    pub(super) input: FormatContext,
    pub(super) stream_catalog: StreamCatalog,
    pub(super) video_stream: StreamInfo,
    pub(super) video_decoder: Decoder,
    pub(super) audio_stream: Option<StreamInfo>,
    pub(super) audio_decoder: Option<Decoder>,
    pub(super) subtitle_stream: Option<StreamInfo>,
    pub(super) subtitle_decoder: Option<Decoder>,
}

struct ProbedPlaybackInput {
    input: FormatContext,
    stream_catalog: StreamCatalog,
    video_stream: StreamInfo,
    audio_stream: Option<StreamInfo>,
    subtitle_stream: Option<StreamInfo>,
    allow_audio_decoder_failure: bool,
}

#[derive(Clone)]
pub(super) struct StreamCatalog {
    streams: Vec<StreamInfo>,
}

impl StreamCatalog {
    #[cfg(test)]
    pub(super) fn empty_for_test() -> Self {
        Self {
            streams: Vec::new(),
        }
    }

    fn from_input(input: &FormatContext) -> std::result::Result<Self, String> {
        Ok(Self {
            streams: input.streams()?,
        })
    }

    fn stream_by_index(
        &self,
        index: usize,
        media_type: ffi::AVMediaType,
    ) -> std::result::Result<StreamInfo, String> {
        let stream = self
            .streams
            .iter()
            .copied()
            .find(|stream| usize::try_from(stream.index).ok() == Some(index))
            .ok_or_else(|| "FFmpeg 媒体流索引越界".to_string())?;
        let codecpar = unsafe { (*stream.stream).codecpar };
        if codecpar.is_null() {
            return Err("FFmpeg 媒体流缺少 codec 参数".to_string());
        }
        if unsafe { (*codecpar).codec_type } != media_type {
            return Err("FFmpeg 媒体流类型与所选轨道不匹配".to_string());
        }
        if stream.decoder.is_null() {
            return Err("FFmpeg 未找到所选媒体流的解码器".to_string());
        }
        Ok(stream)
    }

    pub(super) fn tracks(&self, media_type: ffi::AVMediaType) -> Vec<PlaybackTrack> {
        self.streams
            .iter()
            .filter_map(|stream| {
                let index = usize::try_from(stream.index).ok()?;
                self.stream_by_index(index, media_type).ok()?;
                let codec = ffmpeg_codec_name(stream.codec_id);
                let title = stream_metadata(*stream, c"title");
                let mut track = PlaybackTrack::new(
                    index,
                    title.clone().unwrap_or_else(|| codec.to_uppercase()),
                    false,
                );
                track.language = stream_metadata(*stream, c"language");
                track.title = title;
                track.codec = Some(codec);
                Some(track)
            })
            .collect()
    }
}

pub(super) fn open_playback_input_with_fallback(
    source: &mut FfmpegPlaybackInput,
    control: Arc<FfmpegControl>,
    event_tx: &Sender<BackendEvent>,
) -> std::result::Result<OpenedPlaybackInput, String> {
    let resolved_cache_config = source
        .cache_config
        .clone()
        .resolved_for_cacheable_input(should_cache_http_url(&source.url));
    let mut cached_source = CachedInputSource::new(
        &source.url,
        source.http_headers.as_slice(),
        source.content_length,
        &resolved_cache_config,
        Arc::clone(&control),
        event_tx.clone(),
    )?;
    let initial_probe_profile = initial_probe_profile(source);
    let probed = match probe_playback_input(
        source,
        &cached_source,
        Arc::clone(&control),
        initial_probe_profile,
        false,
    ) {
        Ok(probed)
            if probe_result_satisfies_selection(source, &probed)
                && (initial_probe_profile == InputProbeProfile::Subtitle
                    || !selected_pgs_subtitle_needs_deeper_probe(&probed)) =>
        {
            probed
        }
        Ok(probed) => {
            let fallback_probe_profile = fallback_probe_profile(initial_probe_profile, &probed);
            tracing::debug!(
                initial_probe_profile = ?initial_probe_profile,
                fallback_probe_profile = ?fallback_probe_profile,
                "FFmpeg initial probe did not satisfy selected streams; retrying"
            );
            match probe_playback_input(
                source,
                &cached_source,
                control,
                fallback_probe_profile,
                true,
            ) {
                Ok(probed) => probed,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "FFmpeg probe fallback failed; continuing with initial probe result"
                    );
                    probed
                }
            }
        }
        Err(initial_error) => {
            let fallback_probe_profile = fallback_probe_profile_for_source(source);
            let native_http_fallback =
                cached_source.disable_empty_startup_cache_for_native_fallback();
            if native_http_fallback {
                tracing::warn!(
                    %initial_error,
                    initial_probe_profile = ?initial_probe_profile,
                    fallback_probe_profile = ?fallback_probe_profile,
                    "HTTP stream cache produced no startup byte; retrying through native FFmpeg AVIO"
                );
            } else {
                tracing::debug!(
                    %initial_error,
                    initial_probe_profile = ?initial_probe_profile,
                    fallback_probe_profile = ?fallback_probe_profile,
                    "FFmpeg initial probe failed; retrying"
                );
            }
            probe_playback_input(
                source,
                &cached_source,
                control,
                fallback_probe_profile,
                true,
            )
            .map_err(|fallback_error| {
                format!("FFmpeg 初始探测失败：{initial_error}；重试探测也失败：{fallback_error}")
            })?
        }
    };
    let mut probed = probed;
    probed.input.shutdown_cached_io_on_drop();
    cached_source.release();
    let opened = open_decoders_for_probed_input(probed)?;
    reconcile_input_selection(source, &opened, event_tx);
    let start_position_seconds = initial_position_for_duration(
        source.start_position_seconds,
        opened.input.duration_seconds(),
    );
    if start_position_seconds != source.start_position_seconds {
        tracing::warn!(
            requested_position_seconds = source.start_position_seconds,
            duration_seconds = ?opened.input.duration_seconds(),
            "FFmpeg resume position exceeds actual media duration; starting from the beginning"
        );
        source.start_position_seconds = start_position_seconds;
        let _ = event_tx.send(BackendEvent::new(
            source.session_id,
            BackendEventKind::PositionChanged(start_position_seconds),
        ));
    }
    Ok(opened)
}

fn initial_position_for_duration(position: f64, duration: Option<f64>) -> f64 {
    if !position.is_finite()
        || position < 0.0
        || duration
            .is_some_and(|duration| duration.is_finite() && duration > 0.0 && position >= duration)
    {
        0.0
    } else {
        position
    }
}

fn reconcile_input_selection(
    source: &mut FfmpegPlaybackInput,
    opened: &OpenedPlaybackInput,
    event_tx: &Sender<BackendEvent>,
) {
    let mut selected = source.selected_tracks.clone();
    selected.audio_stream_index = opened
        .audio_stream
        .and_then(|stream| usize::try_from(stream.index).ok());
    if selected.audio_stream_index != source.selected_tracks.audio_stream_index {
        selected.default_audio_stream_index = selected.audio_stream_index;
    }
    if selected.subtitle_external_url.is_none() && opened.subtitle_stream.is_none() {
        selected.set_subtitle_track(None);
    }
    if selected == source.selected_tracks {
        return;
    }

    // The response can be a server warning clip with an entirely different
    // stream layout. Future track switches must use the resolved selection too.
    source.selected_tracks = selected.clone();
    let _ = event_tx.send(BackendEvent::new(
        source.session_id,
        BackendEventKind::PlaybackTracksChanged {
            audio: opened
                .stream_catalog
                .tracks(ffi::AVMediaType::AVMEDIA_TYPE_AUDIO),
            subtitles: opened
                .stream_catalog
                .tracks(ffi::AVMediaType::AVMEDIA_TYPE_SUBTITLE),
            selected,
        },
    ));
}

fn stream_metadata(stream: StreamInfo, key: &CStr) -> Option<String> {
    let entry =
        unsafe { ffi::av_dict_get((*stream.stream).metadata, key.as_ptr(), std::ptr::null(), 0) };
    if entry.is_null() || unsafe { (*entry).value.is_null() } {
        return None;
    }
    let value = unsafe { CStr::from_ptr((*entry).value) }.to_string_lossy();
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

pub(in crate::backend::ffmpeg) fn initial_probe_profile(
    source: &FfmpegPlaybackInput,
) -> InputProbeProfile {
    if selected_internal_pgs_subtitle(source) {
        InputProbeProfile::Subtitle
    } else {
        InputProbeProfile::Fast
    }
}

fn fallback_probe_profile(
    initial_probe_profile: InputProbeProfile,
    probed: &ProbedPlaybackInput,
) -> InputProbeProfile {
    if initial_probe_profile == InputProbeProfile::Subtitle
        || selected_pgs_subtitle_needs_deeper_probe(probed)
    {
        InputProbeProfile::Subtitle
    } else {
        InputProbeProfile::Full
    }
}

fn fallback_probe_profile_for_source(source: &FfmpegPlaybackInput) -> InputProbeProfile {
    if selected_internal_pgs_subtitle(source) {
        InputProbeProfile::Subtitle
    } else {
        InputProbeProfile::Full
    }
}

fn probe_result_satisfies_selection(
    source: &FfmpegPlaybackInput,
    probed: &ProbedPlaybackInput,
) -> bool {
    let audio_satisfied =
        source.selected_tracks.audio_stream_index.is_none() || probed.audio_stream.is_some();
    let subtitle_satisfied = source.selected_tracks.subtitle_stream_index.is_none()
        || source.selected_tracks.subtitle_external_url.is_some()
        || probed.subtitle_stream.is_some();
    audio_satisfied && subtitle_satisfied
}

fn selected_internal_pgs_subtitle(source: &FfmpegPlaybackInput) -> bool {
    source.selected_tracks.subtitle_stream_index.is_some()
        && source.selected_tracks.subtitle_external_url.is_none()
        && source
            .selected_tracks
            .subtitle_codec
            .as_deref()
            .is_some_and(is_pgs_subtitle_codec)
}

pub(super) fn is_pgs_subtitle_codec(codec: &str) -> bool {
    matches!(
        codec.trim().to_ascii_lowercase().as_str(),
        "pgs" | "pgssub" | "hdmv_pgs_subtitle" | "hdmv pgs subtitle"
    )
}

fn selected_pgs_subtitle_needs_deeper_probe(probed: &ProbedPlaybackInput) -> bool {
    probed
        .subtitle_stream
        .as_ref()
        .is_some_and(|stream| stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE)
}

fn probe_playback_input(
    source: &FfmpegPlaybackInput,
    cached_source: &CachedInputSource,
    control: Arc<FfmpegControl>,
    probe_profile: InputProbeProfile,
    allow_track_fallback: bool,
) -> std::result::Result<ProbedPlaybackInput, String> {
    let mut input = FormatContext::open(
        &source.url,
        source.http_headers.as_slice(),
        probe_profile,
        cached_source,
        Arc::clone(&control),
    )?;
    input.find_stream_info()?;
    let stream_catalog = StreamCatalog::from_input(&input)?;

    let video_stream = input
        .best_stream(ffi::AVMediaType::AVMEDIA_TYPE_VIDEO)?
        .ok_or_else(|| "FFmpeg 未找到可解码视频流".to_string())?;
    let audio_stream = select_audio_stream(source, &input, allow_track_fallback)?;
    let subtitle_stream = select_subtitle_stream(source, &input, allow_track_fallback)?;

    Ok(ProbedPlaybackInput {
        input,
        stream_catalog,
        video_stream,
        audio_stream,
        subtitle_stream,
        allow_audio_decoder_failure: allow_track_fallback,
    })
}

fn open_decoders_for_probed_input(
    probed: ProbedPlaybackInput,
) -> std::result::Result<OpenedPlaybackInput, String> {
    let ProbedPlaybackInput {
        input,
        stream_catalog,
        video_stream,
        audio_stream,
        subtitle_stream,
        allow_audio_decoder_failure,
    } = probed;
    let video_decoder = Decoder::open_video(video_stream, HardwareDecodeMode::from_env())
        .map_err(|error| format!("FFmpeg 打开视频解码器失败：{error}"))?;
    let audio_decoder = open_audio_decoder(audio_stream, allow_audio_decoder_failure)?;
    let subtitle_decoder = open_subtitle_decoder(subtitle_stream, video_decoder.size().ok())?;

    Ok(OpenedPlaybackInput {
        input,
        stream_catalog,
        video_stream,
        video_decoder,
        audio_stream,
        audio_decoder,
        subtitle_stream,
        subtitle_decoder,
    })
}

fn select_audio_stream(
    source: &FfmpegPlaybackInput,
    input: &FormatContext,
    allow_track_fallback: bool,
) -> std::result::Result<Option<StreamInfo>, String> {
    select_audio_stream_for_selection(&source.selected_tracks, input, allow_track_fallback)
}

fn select_audio_stream_for_selection(
    selected_tracks: &crate::PlaybackTrackSelection,
    input: &FormatContext,
    allow_track_fallback: bool,
) -> std::result::Result<Option<StreamInfo>, String> {
    let Some(stream_index) = selected_tracks.audio_stream_index else {
        return Ok(None);
    };
    input
        .stream_by_index(stream_index, ffi::AVMediaType::AVMEDIA_TYPE_AUDIO)
        .map(|stream| {
            log_selected_audio_stream(selected_tracks, stream);
            Some(stream)
        })
        .or_else(|error| {
            if allow_track_fallback {
                let stream = input.best_stream(ffi::AVMediaType::AVMEDIA_TYPE_AUDIO)?;
                tracing::warn!(
                    %error,
                    requested_audio_stream_index = stream_index,
                    actual_audio_stream_index = ?stream.map(|stream| stream.index),
                    "FFmpeg selected audio stream unavailable; using actual media audio"
                );
                if let Some(stream) = stream {
                    log_selected_audio_stream(selected_tracks, stream);
                }
                Ok(stream)
            } else {
                Err(format!("FFmpeg 选择指定音频流失败：{error}"))
            }
        })
}

pub(super) fn select_audio_stream_for_selection_from_catalog(
    selected_tracks: &crate::PlaybackTrackSelection,
    catalog: &StreamCatalog,
    allow_audio_decoder_failure: bool,
) -> std::result::Result<Option<StreamInfo>, String> {
    let Some(stream_index) = selected_tracks.audio_stream_index else {
        return Ok(None);
    };
    catalog
        .stream_by_index(stream_index, ffi::AVMediaType::AVMEDIA_TYPE_AUDIO)
        .map(|stream| {
            log_selected_audio_stream(selected_tracks, stream);
            Some(stream)
        })
        .or_else(|error| {
            if allow_audio_decoder_failure {
                tracing::warn!(%error, "FFmpeg selected audio stream unavailable");
                Ok(None)
            } else {
                Err(format!("FFmpeg 选择指定音频流失败：{error}"))
            }
        })
}

fn log_selected_audio_stream(selected_tracks: &crate::PlaybackTrackSelection, stream: StreamInfo) {
    let codec_name = ffmpeg_codec_name(stream.codec_id);
    let (sample_rate, channels) = stream_audio_params(stream);
    tracing::debug!(
        default_audio_stream_index = ?selected_tracks.default_audio_stream_index,
        requested_audio_stream_index = ?selected_tracks.audio_stream_index,
        ffmpeg_audio_stream_index = stream.index,
        audio_codec = %codec_name,
        audio_sample_rate = ?sample_rate,
        audio_channels = ?channels,
        audio_time_base_num = stream.time_base.num,
        audio_time_base_den = stream.time_base.den,
        "selected FFmpeg audio stream"
    );
}

fn ffmpeg_codec_name(codec_id: ffi::AVCodecID) -> String {
    let name = unsafe { ffi::avcodec_get_name(codec_id) };
    if name.is_null() {
        return format!("{codec_id:?}");
    }
    unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned()
}

fn stream_audio_params(stream: StreamInfo) -> (Option<c_int>, Option<c_int>) {
    let codecpar = unsafe { (*stream.stream).codecpar };
    if codecpar.is_null() {
        return (None, None);
    }
    let sample_rate = unsafe { (*codecpar).sample_rate };
    let channels = unsafe { (*codecpar).ch_layout.nb_channels };
    (
        (sample_rate > 0).then_some(sample_rate),
        (channels > 0).then_some(channels),
    )
}

fn select_subtitle_stream(
    source: &FfmpegPlaybackInput,
    input: &FormatContext,
    allow_missing_track: bool,
) -> std::result::Result<Option<StreamInfo>, String> {
    select_subtitle_stream_for_selection(&source.selected_tracks, input, allow_missing_track)
}

fn select_subtitle_stream_for_selection(
    selected_tracks: &crate::PlaybackTrackSelection,
    input: &FormatContext,
    allow_missing_track: bool,
) -> std::result::Result<Option<StreamInfo>, String> {
    if selected_tracks.subtitle_external_url.is_some() {
        return Ok(None);
    }
    let Some(stream_index) = selected_tracks.subtitle_stream_index else {
        return Ok(None);
    };
    input
        .stream_by_index(stream_index, ffi::AVMediaType::AVMEDIA_TYPE_SUBTITLE)
        .map(|stream| {
            log_selected_subtitle_stream(selected_tracks, stream);
            Some(stream)
        })
        .or_else(|error| {
            if allow_missing_track {
                tracing::warn!(
                    %error,
                    requested_subtitle_stream_index = stream_index,
                    "FFmpeg selected subtitle stream unavailable; continuing without subtitles"
                );
                Ok(None)
            } else {
                Err(format!("FFmpeg 选择指定字幕流失败：{error}"))
            }
        })
}

pub(super) fn select_subtitle_stream_for_selection_from_catalog(
    selected_tracks: &crate::PlaybackTrackSelection,
    catalog: &StreamCatalog,
) -> std::result::Result<Option<StreamInfo>, String> {
    if selected_tracks.subtitle_external_url.is_some() {
        return Ok(None);
    }
    let Some(stream_index) = selected_tracks.subtitle_stream_index else {
        return Ok(None);
    };
    catalog
        .stream_by_index(stream_index, ffi::AVMediaType::AVMEDIA_TYPE_SUBTITLE)
        .map(|stream| {
            log_selected_subtitle_stream(selected_tracks, stream);
            Some(stream)
        })
        .map_err(|error| format!("FFmpeg 选择指定字幕流失败：{error}"))
}

fn log_selected_subtitle_stream(
    selected_tracks: &crate::PlaybackTrackSelection,
    stream: StreamInfo,
) {
    tracing::debug!(
        requested_subtitle_stream_index = ?selected_tracks.subtitle_stream_index,
        ffmpeg_subtitle_stream_index = stream.index,
        subtitle_codec = %ffmpeg_codec_name(stream.codec_id),
        subtitle_time_base_num = stream.time_base.num,
        subtitle_time_base_den = stream.time_base.den,
        subtitle_start_nsecs = ?stream.start_nsecs,
        "selected FFmpeg subtitle stream"
    );
}

pub(super) fn open_audio_decoder(
    audio_stream: Option<StreamInfo>,
    allow_audio_decoder_failure: bool,
) -> std::result::Result<Option<Decoder>, String> {
    let Some(stream) = audio_stream else {
        return Ok(None);
    };
    match Decoder::open_audio(stream) {
        Ok(decoder) => Ok(Some(decoder)),
        Err(error) if allow_audio_decoder_failure => {
            tracing::warn!(%error, "FFmpeg audio decoder initialization failed");
            Ok(None)
        }
        Err(error) => Err(format!("FFmpeg 打开音频解码器失败：{error}")),
    }
}

pub(super) fn open_subtitle_decoder(
    subtitle_stream: Option<StreamInfo>,
    video_size: Option<RenderSize>,
) -> std::result::Result<Option<Decoder>, String> {
    let Some(stream) = subtitle_stream else {
        return Ok(None);
    };
    Decoder::open_subtitle(stream, video_size)
        .map(Some)
        .map_err(|error| format!("FFmpeg 打开字幕解码器失败：{error}"))
}

pub(super) fn load_external_subtitle_cue_list(
    selected_tracks: &crate::PlaybackTrackSelection,
    http_headers: &[(String, String)],
) -> std::result::Result<Vec<BackendSubtitleCue>, String> {
    selected_tracks
        .subtitle_external_url
        .as_deref()
        .map(|url| {
            load_external_subtitle_cues(
                url,
                http_headers,
                selected_tracks.subtitle_codec.as_deref(),
            )
            .map(|cues| cues.into_iter().collect::<Vec<_>>())
            .map_err(|error| format!("加载外挂字幕失败：{error}"))
        })
        .transpose()
        .map(|cues| cues.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::initial_position_for_duration;

    #[test]
    fn resume_is_reset_only_when_invalid_or_outside_known_duration() {
        for (position, duration, expected) in [
            (10.0, Some(10.0), 0.0),
            (1200.0, Some(10.0), 0.0),
            (9.5, Some(10.0), 9.5),
            (1200.0, None, 1200.0),
            (1200.0, Some(0.0), 1200.0),
            (1200.0, Some(f64::NAN), 1200.0),
            (0.0, Some(10.0), 0.0),
            (-1.0, Some(10.0), 0.0),
            (f64::NAN, Some(10.0), 0.0),
            (f64::INFINITY, Some(10.0), 0.0),
        ] {
            assert_eq!(initial_position_for_duration(position, duration), expected);
        }
    }
}
