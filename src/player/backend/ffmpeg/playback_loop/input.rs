use std::{
    ffi::CStr,
    os::raw::c_int,
    sync::{Arc, mpsc::Sender},
};

use ffmpeg_sys_next as ffi;

use crate::player::{
    backend::{BackendEvent, BackendSubtitleCue},
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
}

pub(super) fn open_playback_input_with_fallback(
    source: &FfmpegPlaybackInput,
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
            tracing::debug!(
                %initial_error,
                initial_probe_profile = ?initial_probe_profile,
                fallback_probe_profile = ?fallback_probe_profile,
                "FFmpeg initial probe failed; retrying"
            );
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
    open_decoders_for_probed_input(probed)
}

pub(in crate::player::backend::ffmpeg) fn initial_probe_profile(
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
    allow_audio_decoder_failure: bool,
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
    let audio_stream = select_audio_stream(source, &input, allow_audio_decoder_failure)?;
    let subtitle_stream = select_subtitle_stream(source, &input)?;

    Ok(ProbedPlaybackInput {
        input,
        stream_catalog,
        video_stream,
        audio_stream,
        subtitle_stream,
        allow_audio_decoder_failure,
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
    allow_audio_decoder_failure: bool,
) -> std::result::Result<Option<StreamInfo>, String> {
    select_audio_stream_for_selection(&source.selected_tracks, input, allow_audio_decoder_failure)
}

fn select_audio_stream_for_selection(
    selected_tracks: &crate::player::PlaybackTrackSelection,
    input: &FormatContext,
    allow_audio_decoder_failure: bool,
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
            if allow_audio_decoder_failure {
                tracing::warn!(%error, "FFmpeg selected audio stream unavailable");
                Ok(None)
            } else {
                Err(format!("FFmpeg 选择指定音频流失败：{error}"))
            }
        })
}

pub(super) fn select_audio_stream_for_selection_from_catalog(
    selected_tracks: &crate::player::PlaybackTrackSelection,
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

fn log_selected_audio_stream(
    selected_tracks: &crate::player::PlaybackTrackSelection,
    stream: StreamInfo,
) {
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
) -> std::result::Result<Option<StreamInfo>, String> {
    select_subtitle_stream_for_selection(&source.selected_tracks, input)
}

fn select_subtitle_stream_for_selection(
    selected_tracks: &crate::player::PlaybackTrackSelection,
    input: &FormatContext,
) -> std::result::Result<Option<StreamInfo>, String> {
    if selected_tracks.subtitle_external_url.is_some() {
        return Ok(None);
    }
    let Some(stream_index) = selected_tracks.subtitle_stream_index else {
        return Ok(None);
    };
    input
        .stream_by_index(stream_index, ffi::AVMediaType::AVMEDIA_TYPE_SUBTITLE)
        .map(Some)
        .map_err(|error| format!("FFmpeg 选择指定字幕流失败：{error}"))
}

pub(super) fn select_subtitle_stream_for_selection_from_catalog(
    selected_tracks: &crate::player::PlaybackTrackSelection,
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
        .map(Some)
        .map_err(|error| format!("FFmpeg 选择指定字幕流失败：{error}"))
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
    selected_tracks: &crate::player::PlaybackTrackSelection,
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
