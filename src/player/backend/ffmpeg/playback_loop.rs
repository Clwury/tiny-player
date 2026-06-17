use super::*;

#[path = "playback_loop/audio/audio_decode_pipeline.rs"]
mod audio_decode_pipeline;
#[path = "playback_loop/audio/audio_decode_worker.rs"]
mod audio_decode_worker;
#[path = "playback_loop/audio/audio_output_gate.rs"]
mod audio_output_gate;
#[path = "playback_loop/audio/decoded_audio_frame.rs"]
mod decoded_audio_frame;
#[path = "playback_loop/audio/pending_audio_queue.rs"]
mod pending_audio_queue;

#[path = "playback_loop/coordinator/commands.rs"]
mod commands;
#[path = "playback_loop/coordinator/coordinator_commands.rs"]
mod coordinator_commands;
#[path = "playback_loop/coordinator/coordinator_drain.rs"]
mod coordinator_drain;
#[path = "playback_loop/coordinator/coordinator_gate.rs"]
mod coordinator_gate;
#[path = "playback_loop/coordinator/coordinator_tick.rs"]
mod coordinator_tick;
#[path = "playback_loop/coordinator/playback_reset_service.rs"]
mod playback_reset_service;
#[path = "playback_loop/coordinator/playback_services.rs"]
mod playback_services;
#[path = "playback_loop/coordinator/playback_wait_service.rs"]
mod playback_wait_service;
#[path = "playback_loop/coordinator/track_switch.rs"]
mod track_switch;

#[path = "playback_loop/decoder/decode.rs"]
mod decode;
#[path = "playback_loop/decoder/decode_pipeline_service.rs"]
mod decode_pipeline_service;
#[path = "playback_loop/decoder/decoded_output_service.rs"]
mod decoded_output_service;
#[path = "playback_loop/decoder/decoder_drain_service.rs"]
mod decoder_drain_service;
#[path = "playback_loop/decoder/decoder_input_service.rs"]
mod decoder_input_service;
#[path = "playback_loop/decoder/decoder_packet_queue.rs"]
mod decoder_packet_queue;
#[path = "playback_loop/decoder/drain_phase.rs"]
mod drain_phase;

#[path = "playback_loop/demux/demux_cache.rs"]
mod demux_cache;
#[path = "playback_loop/demux/demux_packet_pump.rs"]
mod demux_packet_pump;

#[path = "playback_loop/output/output_drain_service.rs"]
mod output_drain_service;
#[path = "playback_loop/output/output_gate.rs"]
mod output_gate;
#[path = "playback_loop/output/output_gate_service.rs"]
mod output_gate_service;
#[path = "playback_loop/output/output_queue_service.rs"]
mod output_queue_service;
#[path = "playback_loop/output/output_rebuffer.rs"]
mod output_rebuffer;

#[path = "playback_loop/state/playback_block.rs"]
mod playback_block;
#[path = "playback_loop/state/playback_pipeline_state.rs"]
mod playback_pipeline_state;
#[path = "playback_loop/state/playback_snapshot.rs"]
mod playback_snapshot;
#[path = "playback_loop/state/session.rs"]
mod session;
#[path = "playback_loop/state/timeline.rs"]
mod timeline;

#[path = "playback_loop/subtitle/subtitle_decode_worker.rs"]
mod subtitle_decode_worker;
#[path = "playback_loop/subtitle/subtitles.rs"]
mod subtitles;

#[path = "playback_loop/video/decoded_video_frame.rs"]
mod decoded_video_frame;
#[path = "playback_loop/video/scheduled_video_queue.rs"]
mod scheduled_video_queue;
#[path = "playback_loop/video/video_decode_drain_frame_processor.rs"]
mod video_decode_drain_frame_processor;
#[path = "playback_loop/video/video_decode_pipeline.rs"]
mod video_decode_pipeline;
#[path = "playback_loop/video/video_decode_recovery_service.rs"]
mod video_decode_recovery_service;
#[path = "playback_loop/video/video_decode_worker.rs"]
mod video_decode_worker;
#[path = "playback_loop/video/video_frame_admission_service.rs"]
mod video_frame_admission_service;
#[path = "playback_loop/video/video_frame_prepare_admission_service.rs"]
mod video_frame_prepare_admission_service;
#[path = "playback_loop/video/video_frame_prepare_worker.rs"]
mod video_frame_prepare_worker;
#[path = "playback_loop/video/video_output_gate.rs"]
mod video_output_gate;

use super::avio::{CachedInputSource, should_cache_http_url};
use audio_decode_pipeline::AudioDecodePipeline;
#[cfg(test)]
pub(super) use audio_output_gate::{
    PendingAudioUnderrunRecoveryPlan, discard_stale_pending_audio_before_recovery_start,
    pending_audio_underrun_recovery_plan,
};
use coordinator_commands::{
    PlaybackCommandContext, PlaybackCommandServiceStatus, service_playback_commands,
};
use coordinator_drain::{
    PlaybackEofDrainContext, PlaybackEofDrainStatus, service_playback_eof_drain,
};
use coordinator_gate::PlaybackCoordinatorGateContext;
use coordinator_tick::{PlaybackTickContext, PlaybackTickStatus, service_playback_tick};
use decode::PlaybackGeneration;
#[cfg(test)]
pub(super) use decoded_video_frame::{
    DecodedVideoFrameStartAction, decoded_video_frame_start_action,
};
pub(super) use demux_cache::DemuxReaderWatermark;
use demux_cache::{DemuxPacketCache, DemuxPacketCacheInput, DemuxReadResult};
pub(super) use output_gate::{PlaybackOutputScheduler, PlaybackOutputSnapshot};
pub(super) use output_rebuffer::PlaybackOutputState;
#[cfg(test)]
pub(super) use output_rebuffer::{
    AudioClockResumeDecision, RebufferResumeAnchor, audio_clock_resume_decision,
    audio_clock_resume_timeline_nsecs, audio_output_buffered_until_for_resume,
    decoded_audio_forward_nsecs_from, decoded_video_start_prebuffer_reached,
    demux_reader_ready_for_output, initial_audio_clock_resume_decision, playback_resume_waterline,
    playback_resume_waterline_blocked_on, rebuffer_audio_clock_resume_decision,
    rebuffer_playback_resume_waterline, rebuffer_playback_resume_waterline_after_prolonged_wait,
    rebuffer_playback_resume_waterline_with_resource_pressure, should_block_for_demux_read,
    video_decode_should_skip_nonref_for_pressure, video_output_rebuffer_resume_duration,
    video_output_rebuffer_resume_duration_with_resource_pressure,
    video_output_rebuffer_resume_reached, video_output_rebuffer_should_enter,
};
#[cfg(test)]
pub(super) use pending_audio_queue::PendingStartAudio;
pub(super) use playback_block::PlaybackBlockReason;
use playback_pipeline_state::PlaybackPipelineState;
use playback_services::PlaybackPipelineServices;
#[cfg(test)]
pub(super) use scheduled_video_queue::{
    audio_clocked_video_wait_duration, discard_queued_video_before, pop_audio_clocked_video_frame,
    pop_audio_clocked_video_frame_with_policy, push_queued_video_frame,
    queued_video_buffered_until_nsecs, queued_video_frame_ready_for_audio_clock,
    should_drop_late_video_frame, video_output_rebuffer_low_water,
};
use session::PlaybackSession;
use subtitles::{SubtitleDecodeContext, SubtitlePipeline};
use timeline::reset_playback_timeline_state;
use video_decode_pipeline::VideoDecodePipeline;
pub(super) use video_decode_pipeline::VideoDecodeRecovery;
#[cfg(test)]
pub(super) use video_decode_pipeline::video_decode_error_is_recoverable;
use video_decode_worker::VideoDecodeWorkerInfo;
use video_frame_prepare_worker::VideoFramePrepareWorker;
#[cfg(test)]
pub(super) use video_output_gate::admit_decoded_video_frame_to_vo;

const END_OF_PLAYBACK_READ_ERROR_TOLERANCE_SECONDS: f64 = 2.0;
const CORRUPT_VIDEO_FRAME_RECOVERY_ERROR: &str = "__tiny_corrupt_video_frame_recovery__";
pub(super) const VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS: u64 = 240;
// Low-level seeks already snap backward to a keyframe; keep this small so seek-forward
// shortcuts do not spend several seconds decoding and dropping HEVC preroll frames.
pub(super) const HEVC_SEEK_PREROLL_NSECS: u64 = 1_000_000_000;
pub(super) const HEVC_CACHED_SEEK_PREROLL_NSECS: u64 = 5_000_000_000;

struct OpenedPlaybackInput {
    input: FormatContext,
    stream_catalog: StreamCatalog,
    video_stream: StreamInfo,
    video_decoder: Decoder,
    audio_stream: Option<StreamInfo>,
    audio_decoder: Option<Decoder>,
    subtitle_stream: Option<StreamInfo>,
    subtitle_decoder: Option<Decoder>,
}

struct ProbedPlaybackInput {
    input: FormatContext,
    stream_catalog: StreamCatalog,
    video_stream: StreamInfo,
    audio_stream: Option<StreamInfo>,
    subtitle_stream: Option<StreamInfo>,
    allow_audio_decoder_failure: bool,
}

fn video_seek_preroll_nsecs(codec_id: ffi::AVCodecID) -> u64 {
    match codec_id {
        ffi::AVCodecID::AV_CODEC_ID_HEVC => HEVC_SEEK_PREROLL_NSECS,
        _ => 0,
    }
}

fn video_cached_seek_preroll_nsecs(codec_id: ffi::AVCodecID) -> u64 {
    match codec_id {
        ffi::AVCodecID::AV_CODEC_ID_HEVC => HEVC_CACHED_SEEK_PREROLL_NSECS,
        _ => 0,
    }
}

fn preroll_seek_position_seconds(codec_id: ffi::AVCodecID, position_seconds: f64) -> f64 {
    let position_seconds = position_seconds.max(0.0);
    let preroll_seconds = nsecs_to_seconds(video_seek_preroll_nsecs(codec_id));
    (position_seconds - preroll_seconds).max(0.0)
}

#[cfg(test)]
mod seek_preroll_tests {
    use super::*;

    #[test]
    fn hevc_low_level_seek_uses_shorter_preroll_than_cached_seek() {
        assert_eq!(
            video_seek_preroll_nsecs(ffi::AVCodecID::AV_CODEC_ID_HEVC),
            1_000_000_000
        );
        assert_eq!(
            video_cached_seek_preroll_nsecs(ffi::AVCodecID::AV_CODEC_ID_HEVC),
            5_000_000_000
        );
        assert!(
            (preroll_seek_position_seconds(ffi::AVCodecID::AV_CODEC_ID_HEVC, 62.36) - 61.36).abs()
                < f64::EPSILON
        );
    }
}

#[derive(Clone)]
struct StreamCatalog {
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

fn open_playback_input_with_fallback(
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

pub(super) fn initial_probe_profile(source: &FfmpegPlaybackInput) -> InputProbeProfile {
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

fn select_audio_stream_for_selection_from_catalog(
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

fn select_subtitle_stream_for_selection_from_catalog(
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

fn open_audio_decoder(
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

fn open_subtitle_decoder(
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

fn load_external_subtitle_cue_list(
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

fn frame_rate_from_duration(frame_duration_nsecs: Option<u64>) -> Option<f64> {
    let duration = frame_duration_nsecs?;
    if duration == 0 {
        return None;
    }
    Some(1_000_000_000.0 / duration as f64)
}

fn playback_video_info_from_worker(
    video_stream: StreamInfo,
    video_decoder: &VideoDecodeWorkerInfo,
) -> Option<PlaybackVideoInfo> {
    Some(PlaybackVideoInfo {
        decoder: video_decoder.decoder_name.clone(),
        size: video_decoder.size?,
        frame_rate: frame_rate_from_duration(video_stream.frame_duration_nsecs),
        hardware_accelerated: video_decoder.hardware_accelerated,
    })
}

pub(super) fn run_ffmpeg_playback(
    mut source: FfmpegPlaybackInput,
    video_output_queue: VideoOutputQueue,
    event_tx: Sender<BackendEvent>,
    control: Arc<FfmpegControl>,
    command_rx: Receiver<FfmpegCommand>,
    frame_presented: Arc<AtomicBool>,
) -> std::result::Result<(), String> {
    let mut session = PlaybackSession::new(source.session_id, source.start_position_seconds);
    control.set_session_id(session.id());
    let OpenedPlaybackInput {
        mut input,
        stream_catalog,
        video_stream,
        video_decoder,
        audio_stream,
        audio_decoder: opened_audio_decoder,
        subtitle_stream,
        subtitle_decoder,
    } = open_playback_input_with_fallback(&source, Arc::clone(&control), &event_tx)?;
    let video_decode_pipeline = VideoDecodePipeline::spawn(video_decoder)?;
    let initial_playback_video_info =
        playback_video_info_from_worker(video_stream, video_decode_pipeline.info());
    let playback_generation = PlaybackGeneration::default();
    if let Some(device) = video_decode_pipeline.info().vulkan_device.clone() {
        video_output_queue.request_vulkan_prewarm(session.id(), device);
    }
    if source.start_position_seconds > 0.0 {
        let seek_position_seconds =
            preroll_seek_position_seconds(video_stream.codec_id, source.start_position_seconds);
        tracing::debug!(
            target_position_seconds = source.start_position_seconds,
            seek_position_seconds,
            preroll_nsecs = video_seek_preroll_nsecs(video_stream.codec_id),
            codec = ?video_stream.codec_id,
            "applying FFmpeg initial seek preroll"
        );
        input.seek_stream(video_stream, seek_position_seconds)?;
    }
    let duration_seconds = input.duration_seconds();
    let http_cache = input.cached_io_cache();
    if let Some(cache) = &http_cache {
        cache.set_duration_seconds(duration_seconds);
    }
    let input_cacheable = should_cache_http_url(&source.url);
    let demux_cache_config = source
        .cache_config
        .clone()
        .resolved_for_cacheable_input(input_cacheable);
    let should_wait_initial_demux_cache = demux_cache_config.demuxer_cache_wait;
    let demux_cache = DemuxPacketCache::spawn(
        DemuxPacketCacheInput {
            input,
            video_stream,
            audio_stream,
            subtitle_stream,
            duration_seconds,
            start_position_seconds: source.start_position_seconds,
            session_id: session.id(),
            cache_config: demux_cache_config,
        },
        Arc::clone(&control),
        event_tx.clone(),
    )?;
    let video_frame_prepare_worker =
        VideoFramePrepareWorker::spawn(video_output_queue.buffer_pool())?;
    let current_start_position_nsecs = session.start_position_nsecs();
    let video_frame_duration_nsecs = video_stream
        .frame_duration_nsecs
        .unwrap_or(DEFAULT_VIDEO_FRAME_DURATION_NSECS);
    let playback_timeline_origin_nsecs = video_stream.start_nsecs;
    let video_clock = TimestampMapper::new(
        video_stream.start_nsecs,
        current_start_position_nsecs,
        Some(video_frame_duration_nsecs),
    );
    let scheduler = PlaybackScheduler::new(current_start_position_nsecs);
    let position_reporter = PositionReporter::default();
    let dovi_pipeline = DoviPipeline::default();
    let subtitle_pipeline = SubtitlePipeline::new(
        subtitle_stream,
        subtitle_decoder,
        &source,
        current_start_position_nsecs,
    )?;

    let mut audio_output = None;
    let mut audio_decode_pipeline = None;
    if let Some(decoder) = opened_audio_decoder {
        match AudioOutput::new(Arc::clone(&control)) {
            Ok(output) => {
                match AudioDecodePipeline::spawn(decoder, output.sample_rate(), output.channels()) {
                    Ok(worker) => {
                        let audio_info = worker.info();
                        tracing::debug!(
                            sample_rate = audio_info.output_rate,
                            channels = audio_info.output_channels,
                            "initialized native FFmpeg audio output and decode worker"
                        );
                        audio_output = Some(output);
                        audio_decode_pipeline = Some(worker);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "FFmpeg audio decode worker initialization failed");
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "native audio output initialization failed; playing video without audio");
            }
        }
    }
    if should_wait_initial_demux_cache {
        tracing::debug!(
            session_id = ?session.id(),
            "waiting for initial FFmpeg demux cache fill before playback restart"
        );
        demux_cache.wait_until_initial_cache_fill()?;
    }
    let audio_clock = TimestampMapper::new(
        audio_stream.and_then(|stream| stream.start_nsecs),
        current_start_position_nsecs,
        None,
    );
    if let Some(output) = &audio_output {
        output.reset_clock(current_start_position_nsecs);
    }

    if let Some(duration) = duration_seconds {
        let _ = event_tx.send(BackendEvent::new(
            session.id(),
            BackendEventKind::DurationChanged(duration),
        ));
    }
    let _ = event_tx.send(BackendEvent::new(
        session.id(),
        BackendEventKind::PlaybackInfoChanged(initial_playback_video_info),
    ));
    let emit_playback_buffered_events = false;
    let buffered_reporter =
        BufferedReporter::new_with_events(audio_output.is_some(), emit_playback_buffered_events);
    let output_scheduler = PlaybackOutputScheduler::new();
    let mut video_decode_recovery = VideoDecodeRecovery::default();
    video_decode_recovery
        .reset_for_timeline_start(video_stream.codec_id, current_start_position_nsecs);
    let mut pipeline_services = PlaybackPipelineServices::default();
    let mut pipeline = PlaybackPipelineState {
        video_stream,
        video_frame_duration_nsecs,
        video_decode_pipeline,
        audio_decode_pipeline,
        subtitle_pipeline,
        video_decode_recovery,
        playback_generation,
        audio_stream,
        decoded_video_frame_count: 0,
        dropped_video_frames_before_start_count: 0,
        dropped_audio_frames_before_start_count: 0,
        video_clock,
        playback_timeline_origin_nsecs,
        audio_clock,
        audio_output,
        scheduler,
        output_scheduler,
        dovi_pipeline,
        buffered_reporter,
        position_reporter,
        video_frame_prepare_worker,
        current_start_position_nsecs,
        video_packet_count: 0,
        video_decode_skip_nonref_active: false,
    };
    pipeline.buffered_reporter.reset_to(
        source.start_position_seconds.max(0.0),
        session.id(),
        &event_tx,
    );
    let _ = event_tx.send(BackendEvent::new(
        session.id(),
        BackendEventKind::Buffering(true),
    ));
    let _ = event_tx.send(BackendEvent::new(
        session.id(),
        BackendEventKind::SubtitleChanged(None),
    ));

    'playback_coordinator: loop {
        while !control.should_stop() {
            match service_playback_commands(PlaybackCommandContext {
                source: &mut source,
                session: &mut session,
                control: &control,
                command_rx: &command_rx,
                http_cache: http_cache.as_ref(),
                stream_catalog: &stream_catalog,
                demux_cache: &demux_cache,
                vo_queue: &video_output_queue,
                pipeline: &mut pipeline,
                emit_playback_buffered_events,
                event_tx: &event_tx,
            })? {
                PlaybackCommandServiceStatus::Idle => {}
                PlaybackCommandServiceStatus::Continue => continue,
                PlaybackCommandServiceStatus::Stopped => break,
            }

            if pipeline_services
                .coordinator_gate
                .service(PlaybackCoordinatorGateContext {
                    control: &control,
                    output_scheduler: &pipeline.output_scheduler,
                    scheduler: &mut pipeline.scheduler,
                    playback_wait: &pipeline_services.wait,
                })
                .should_continue()
            {
                continue;
            }

            match service_playback_tick(PlaybackTickContext {
                session_id: session.id(),
                demux_cache: &demux_cache,
                services: &mut pipeline_services,
                pipeline: &mut pipeline,
                control: &control,
                event_tx: &event_tx,
                vo_queue: &video_output_queue,
                frame_presented: &frame_presented,
            })? {
                PlaybackTickStatus::Continue => continue,
                PlaybackTickStatus::Eof | PlaybackTickStatus::Stopped => break,
            }
        }

        if control.should_stop() {
            return Ok(());
        }
        match service_playback_eof_drain(PlaybackEofDrainContext {
            session_id: session.id(),
            duration_seconds,
            demux_cache: &demux_cache,
            services: &mut pipeline_services,
            pipeline: &mut pipeline,
            control: &control,
            event_tx: &event_tx,
            vo_queue: &video_output_queue,
            frame_presented: &frame_presented,
        })? {
            PlaybackEofDrainStatus::Complete | PlaybackEofDrainStatus::Stopped => return Ok(()),
            PlaybackEofDrainStatus::SeekPending => continue 'playback_coordinator,
        }
    }
}

#[cfg(test)]
pub(super) fn playback_read_finished(
    read_result: c_int,
    duration_seconds: Option<f64>,
    buffered_until_seconds: Option<f64>,
) -> bool {
    read_result == ffi::AVERROR_EOF
        || (read_result == ffi::AVERROR(ffi::EIO)
            && playback_buffered_near_duration(duration_seconds, buffered_until_seconds))
}

fn playback_buffered_near_duration(
    duration_seconds: Option<f64>,
    buffered_until_seconds: Option<f64>,
) -> bool {
    let Some(duration_seconds) = duration_seconds.filter(|duration| duration.is_finite()) else {
        return false;
    };
    let Some(buffered_until_seconds) =
        buffered_until_seconds.filter(|buffered_until| buffered_until.is_finite())
    else {
        return false;
    };

    duration_seconds > 0.0
        && buffered_until_seconds + END_OF_PLAYBACK_READ_ERROR_TOLERANCE_SECONDS >= duration_seconds
}

fn packet_duration_nsecs(packet: &AvPacket, stream: StreamInfo) -> Option<u64> {
    packet
        .duration()
        .and_then(|duration| timestamp_to_nsecs(duration, stream.time_base))
}

fn push_subtitle_cue(cues: &mut VecDeque<BackendSubtitleCue>, cue: BackendSubtitleCue) {
    if !cue.has_content() || cue.end_nsecs <= cue.start_nsecs {
        return;
    }
    let index = cues
        .iter()
        .position(|current| current.start_nsecs > cue.start_nsecs)
        .unwrap_or(cues.len());
    cues.insert(index, cue);
}

pub(super) fn trim_overlapping_subtitle_cues_at(
    cues: &mut VecDeque<BackendSubtitleCue>,
    trim_nsecs: u64,
) {
    for cue in cues.iter_mut() {
        if cue.has_content() && cue.start_nsecs < trim_nsecs && trim_nsecs < cue.end_nsecs {
            cue.end_nsecs = trim_nsecs;
        }
    }
    cues.retain(|cue| cue.has_content() && cue.end_nsecs > cue.start_nsecs);
}

fn refresh_playback_timeline_origin(
    playback_timeline_origin_nsecs: &mut Option<u64>,
    video_clock: &TimestampMapper,
    subtitle_stream: Option<StreamInfo>,
    subtitle_cues: &mut VecDeque<BackendSubtitleCue>,
) {
    let next_origin_nsecs = video_clock.timeline_origin_nsecs();
    if *playback_timeline_origin_nsecs == next_origin_nsecs {
        return;
    }

    if subtitle_stream
        .is_some_and(|stream| stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE)
    {
        rebase_subtitle_cues_to_timeline_origin(
            subtitle_cues,
            *playback_timeline_origin_nsecs,
            next_origin_nsecs,
        );
    }
    *playback_timeline_origin_nsecs = next_origin_nsecs;
}

pub(super) fn rebase_subtitle_cues_to_timeline_origin(
    cues: &mut VecDeque<BackendSubtitleCue>,
    previous_origin_nsecs: Option<u64>,
    next_origin_nsecs: Option<u64>,
) {
    let previous_origin_nsecs = previous_origin_nsecs.unwrap_or(0);
    let next_origin_nsecs = next_origin_nsecs.unwrap_or(0);
    if previous_origin_nsecs == next_origin_nsecs {
        return;
    }

    if next_origin_nsecs > previous_origin_nsecs {
        let delta = next_origin_nsecs - previous_origin_nsecs;
        for cue in cues.iter_mut() {
            cue.start_nsecs = cue.start_nsecs.saturating_sub(delta);
            cue.end_nsecs = cue.end_nsecs.saturating_sub(delta);
        }
    } else {
        let delta = previous_origin_nsecs - next_origin_nsecs;
        for cue in cues.iter_mut() {
            cue.start_nsecs = cue.start_nsecs.saturating_add(delta);
            cue.end_nsecs = cue.end_nsecs.saturating_add(delta);
        }
    }
    cues.retain(|cue| cue.has_content() && cue.end_nsecs > cue.start_nsecs);
}

fn subtitle_cue_queue_from_external(
    cues: &[BackendSubtitleCue],
    start_position_nsecs: u64,
) -> VecDeque<BackendSubtitleCue> {
    cues.iter()
        .filter(|cue| cue.end_nsecs >= start_position_nsecs)
        .cloned()
        .collect()
}

pub(super) fn subtitle_cue_timeline_nsecs(
    cue_pts_nsecs: Option<u64>,
    packet_timestamp: Option<i64>,
    stream: StreamInfo,
    playback_timeline_origin_nsecs: Option<u64>,
) -> Option<u64> {
    let stream_start_nsecs =
        subtitle_stream_timeline_origin(stream, playback_timeline_origin_nsecs);
    if let Some(packet_nsecs) =
        packet_timestamp.and_then(|timestamp| timestamp_to_nsecs(timestamp, stream.time_base))
    {
        return Some(subtitle_timestamp_to_timeline_nsecs(
            packet_nsecs,
            stream_start_nsecs,
        ));
    }
    cue_pts_nsecs
        .map(|pts_nsecs| subtitle_timestamp_to_timeline_nsecs(pts_nsecs, stream_start_nsecs))
}

fn subtitle_stream_timeline_origin(
    stream: StreamInfo,
    playback_timeline_origin_nsecs: Option<u64>,
) -> Option<u64> {
    if stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE {
        playback_timeline_origin_nsecs.or(stream.start_nsecs)
    } else {
        stream.start_nsecs
    }
}

pub(super) fn subtitle_timestamp_to_timeline_nsecs(
    timestamp_nsecs: u64,
    stream_start_nsecs: Option<u64>,
) -> u64 {
    timestamp_nsecs.saturating_sub(stream_start_nsecs.unwrap_or(0))
}

fn update_subtitle_overlay(
    position_nsecs: u64,
    cues: &mut VecDeque<BackendSubtitleCue>,
    active: &mut Option<BackendSubtitleCue>,
    session_id: PlaybackSessionId,
    event_tx: &Sender<BackendEvent>,
) {
    while cues
        .front()
        .is_some_and(|cue| cue.end_nsecs <= position_nsecs)
    {
        cues.pop_front();
    }
    let next = cues
        .iter()
        .find(|cue| cue.start_nsecs <= position_nsecs && position_nsecs < cue.end_nsecs)
        .cloned();
    if *active == next {
        return;
    }
    *active = next.clone();
    let _ = event_tx.send(BackendEvent::new(
        session_id,
        BackendEventKind::SubtitleChanged(next),
    ));
}
