use super::*;

mod commands;
mod decode;
mod demux_cache;
mod session;
mod subtitles;
mod timeline;

use super::avio::{CachedInputSource, should_cache_http_url};
use commands::{begin_seek, begin_track_switch};
use decode::{flush_playback_decode_state, should_drop_backlogged_vulkan_frame};
use demux_cache::{DemuxPacketCache, DemuxPacketCacheInput, DemuxReadResult};
use session::PlaybackSession;
use subtitles::{SubtitleDecodeContext, SubtitlePipeline};
use timeline::reset_playback_timeline_state;

const END_OF_PLAYBACK_READ_ERROR_TOLERANCE_SECONDS: f64 = 2.0;
const CORRUPT_VIDEO_FRAME_RECOVERY_ERROR: &str = "__tiny_corrupt_video_frame_recovery__";
pub(super) const VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS: u64 = 240;
pub(super) const HEVC_SEEK_PREROLL_NSECS: u64 = 5_000_000_000;

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

#[derive(Default)]
pub(super) struct VideoDecodeRecovery {
    waiting_for_keyframe: bool,
    realign_on_next_frame: bool,
    realign_after_recovery_point: bool,
    skipped_packets: u64,
}

impl VideoDecodeRecovery {
    pub(super) fn reset(&mut self) {
        self.waiting_for_keyframe = false;
        self.realign_on_next_frame = false;
        self.realign_after_recovery_point = false;
        self.skipped_packets = 0;
    }

    pub(super) fn reset_for_timeline_start(
        &mut self,
        codec_id: ffi::AVCodecID,
        current_start_position_nsecs: u64,
    ) {
        self.reset();
        if codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC && current_start_position_nsecs > 0 {
            self.begin_with_realign(false);
        }
    }

    pub(super) fn waiting_for_keyframe(&self) -> bool {
        self.waiting_for_keyframe
    }

    pub(super) fn should_skip_packet(&self, packet: &AvPacket, codec_id: ffi::AVCodecID) -> bool {
        if !self.waiting_for_keyframe || packet_is_video_decode_recovery_point(packet, codec_id) {
            return false;
        }
        codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
            || self.skipped_packets < VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS
    }

    pub(super) fn record_skipped_packet(&mut self) -> u64 {
        self.skipped_packets = self.skipped_packets.saturating_add(1);
        self.skipped_packets
    }

    pub(super) fn accept_recovery_point(
        &mut self,
        packet: &AvPacket,
        codec_id: ffi::AVCodecID,
    ) -> bool {
        if !self.waiting_for_keyframe || !packet_is_video_decode_recovery_point(packet, codec_id) {
            return false;
        }

        self.waiting_for_keyframe = false;
        self.realign_on_next_frame = self.realign_after_recovery_point;
        self.realign_after_recovery_point = false;
        self.skipped_packets = 0;
        true
    }

    pub(super) fn accept_after_wait_limit(&mut self, codec_id: ffi::AVCodecID) -> bool {
        if codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC {
            return false;
        }
        if !self.waiting_for_keyframe
            || self.skipped_packets < VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS
        {
            return false;
        }

        self.waiting_for_keyframe = false;
        self.realign_on_next_frame = self.realign_after_recovery_point;
        self.realign_after_recovery_point = false;
        self.skipped_packets = 0;
        true
    }

    pub(super) fn take_realign_on_next_frame(&mut self) -> bool {
        let realign = self.realign_on_next_frame;
        self.realign_on_next_frame = false;
        realign
    }

    pub(super) fn begin_with_realign(&mut self, realign_after_recovery_point: bool) {
        self.waiting_for_keyframe = true;
        self.realign_on_next_frame = false;
        self.realign_after_recovery_point = realign_after_recovery_point;
        self.skipped_packets = 0;
    }
}

fn packet_is_video_decode_recovery_point(packet: &AvPacket, codec_id: ffi::AVCodecID) -> bool {
    if codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC {
        return packet_is_video_seek_point(packet, codec_id);
    }
    packet_is_video_recovery_point(packet, codec_id)
}

fn video_seek_preroll_nsecs(codec_id: ffi::AVCodecID) -> u64 {
    match codec_id {
        ffi::AVCodecID::AV_CODEC_ID_HEVC => HEVC_SEEK_PREROLL_NSECS,
        _ => 0,
    }
}

fn preroll_seek_position_seconds(codec_id: ffi::AVCodecID, position_seconds: f64) -> f64 {
    let position_seconds = position_seconds.max(0.0);
    let preroll_seconds = nsecs_to_seconds(video_seek_preroll_nsecs(codec_id));
    (position_seconds - preroll_seconds).max(0.0)
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
    let mut cached_source = CachedInputSource::new(
        &source.url,
        source.http_headers.as_slice(),
        source.content_length,
        &source.cache_config,
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
        .map(Some)
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
        .map(Some)
        .or_else(|error| {
            if allow_audio_decoder_failure {
                tracing::warn!(%error, "FFmpeg selected audio stream unavailable");
                Ok(None)
            } else {
                Err(format!("FFmpeg 选择指定音频流失败：{error}"))
            }
        })
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

fn playback_video_info(
    video_stream: StreamInfo,
    video_decoder: &Decoder,
) -> Option<PlaybackVideoInfo> {
    Some(PlaybackVideoInfo {
        decoder: video_decoder.decoder_name(),
        size: video_decoder.size().ok()?,
        frame_rate: frame_rate_from_duration(video_stream.frame_duration_nsecs),
        hardware_accelerated: video_decoder.is_hardware_accelerated(),
    })
}

pub(super) fn run_ffmpeg_playback(
    mut source: FfmpegPlaybackInput,
    frame_slot: FrameSlot,
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
        mut audio_stream,
        audio_decoder: opened_audio_decoder,
        subtitle_stream,
        subtitle_decoder,
    } = open_playback_input_with_fallback(&source, Arc::clone(&control), &event_tx)?;
    if let Some(device) = video_decoder.vulkan_device() {
        frame_slot.request_vulkan_prewarm(session.id(), device);
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
    let mut video_frame = AvFrame::new()?;
    let mut video_converter = VideoFrameConverter::new(frame_slot.buffer_pool());
    let mut current_start_position_nsecs = session.start_position_nsecs();
    let video_frame_duration_nsecs = video_stream
        .frame_duration_nsecs
        .unwrap_or(DEFAULT_VIDEO_FRAME_DURATION_NSECS);
    let mut playback_timeline_origin_nsecs = video_stream.start_nsecs;
    let mut video_clock = TimestampMapper::new(
        video_stream.start_nsecs,
        current_start_position_nsecs,
        Some(video_frame_duration_nsecs),
    );
    let mut scheduler = PlaybackScheduler::new(current_start_position_nsecs);
    let mut position_reporter = PositionReporter::default();
    let mut dovi_pipeline = DoviPipeline::default();
    let mut subtitle_pipeline = SubtitlePipeline::new(
        subtitle_stream,
        subtitle_decoder,
        &source,
        current_start_position_nsecs,
    )?;

    let mut audio_output = None;
    let mut audio_decoder = None;
    let mut audio_frame = None;
    let mut audio_resampler = None;
    if let Some(decoder) = opened_audio_decoder {
        match AudioOutput::new(Arc::clone(&control)) {
            Ok(output) => match AudioResampler::new(output.sample_rate(), output.channels()) {
                Ok(resampler) => {
                    tracing::debug!(
                        sample_rate = output.sample_rate(),
                        channels = output.channels(),
                        "initialized native FFmpeg audio output"
                    );
                    audio_frame = Some(AvFrame::new()?);
                    audio_resampler = Some(resampler);
                    audio_output = Some(output);
                    audio_decoder = Some(decoder);
                }
                Err(error) => {
                    tracing::warn!(%error, "FFmpeg audio resampler initialization failed");
                }
            },
            Err(error) => {
                tracing::warn!(%error, "native audio output initialization failed; playing video without audio");
            }
        }
    }
    demux_cache.set_output_underrun_detection_enabled(audio_output.is_some());
    if source.cache_config.demuxer_cache_wait {
        tracing::debug!(
            session_id = ?session.id(),
            "waiting for initial FFmpeg demux cache fill before playback restart"
        );
        demux_cache.wait_until_initial_cache_fill()?;
    }
    let mut audio_clock = TimestampMapper::new(
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
        BackendEventKind::PlaybackInfoChanged(playback_video_info(video_stream, &video_decoder)),
    ));
    let mut packet = AvPacket::new()?;
    let emit_playback_buffered_events = false;
    let mut buffered_reporter =
        BufferedReporter::new_with_events(audio_output.is_some(), emit_playback_buffered_events);
    let mut queued_video_frames = VecDeque::new();
    let mut first_video_frame_pending = true;
    let mut video_decode_recovery = VideoDecodeRecovery::default();
    video_decode_recovery
        .reset_for_timeline_start(video_stream.codec_id, current_start_position_nsecs);
    let mut video_packet_count = 0u64;
    let mut decoded_video_frame_count = 0u64;
    let mut dropped_video_frames_before_start_count = 0u64;
    let mut dropped_audio_frames_before_start_count = 0u64;
    let mut dropped_audio_frames_waiting_for_video_count = 0u64;
    buffered_reporter.reset_to(
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

    while !control.should_stop() {
        let drained_commands = drain_playback_commands(&command_rx, &control);
        if control.should_stop() {
            break;
        }

        if let Some(cache_config) = drained_commands.cache_config {
            source.cache_config = cache_config.clone();
            let demux_cache_config = cache_config
                .clone()
                .resolved_for_cacheable_input(should_cache_http_url(&source.url));
            demux_cache.apply_cache_config(demux_cache_config);
            if let Some(cache) = &http_cache {
                cache.apply_cache_config(&cache_config);
            }
        }

        if let Some(pending_track_selection) = drained_commands.pending_track_selection {
            let position_seconds =
                begin_track_switch(&mut session, &control, &pending_track_selection);
            let switch_result: std::result::Result<(), String> = (|| {
                current_start_position_nsecs = session.start_position_nsecs();
                let demux_seek_result = demux_cache.seek(
                    position_seconds,
                    session.id(),
                    pending_track_selection.generation,
                );
                tracing::debug!(
                    session_id = ?session.id(),
                    position_seconds,
                    generation = pending_track_selection.generation,
                    current_start_position_nsecs,
                    ?demux_seek_result,
                    selected_tracks = ?pending_track_selection.selected_tracks,
                    "handling FFmpeg playback track selection seek"
                );
                control.finish_seek(pending_track_selection.generation);
                source.selected_tracks = pending_track_selection.selected_tracks;

                flush_playback_decode_state(
                    &video_decoder,
                    audio_decoder.as_ref(),
                    &mut subtitle_pipeline,
                    &mut video_frame,
                    audio_frame.as_mut(),
                    &mut packet,
                );

                let previous_audio_output = audio_output.take();
                audio_decoder = None;
                audio_frame = None;
                audio_resampler = None;
                audio_stream = select_audio_stream_for_selection_from_catalog(
                    &source.selected_tracks,
                    &stream_catalog,
                    false,
                )?;
                if let Some(decoder) = open_audio_decoder(audio_stream, false)? {
                    let output = match previous_audio_output {
                        Some(output) => Some(output),
                        None => match AudioOutput::new(Arc::clone(&control)) {
                            Ok(output) => Some(output),
                            Err(error) => {
                                tracing::warn!(%error, "native audio output initialization failed; playing video without audio");
                                None
                            }
                        },
                    };
                    if let Some(output) = output {
                        match AudioResampler::new(output.sample_rate(), output.channels()) {
                            Ok(resampler) => {
                                audio_frame = Some(AvFrame::new()?);
                                output.reset_clock(current_start_position_nsecs);
                                audio_resampler = Some(resampler);
                                audio_output = Some(output);
                                audio_decoder = Some(decoder);
                            }
                            Err(error) => {
                                tracing::warn!(%error, "FFmpeg audio resampler initialization failed");
                            }
                        }
                    }
                }
                demux_cache.set_output_underrun_detection_enabled(audio_output.is_some());

                subtitle_pipeline.switch_tracks(
                    &source,
                    &stream_catalog,
                    video_decoder.size().ok(),
                    current_start_position_nsecs,
                )?;

                reset_playback_timeline_state(
                    video_stream,
                    audio_stream,
                    video_frame_duration_nsecs,
                    current_start_position_nsecs,
                    &mut video_clock,
                    &mut playback_timeline_origin_nsecs,
                    &mut audio_clock,
                    &mut scheduler,
                    None,
                    &mut queued_video_frames,
                    &mut first_video_frame_pending,
                    &mut dovi_pipeline,
                );
                dropped_video_frames_before_start_count = 0;
                dropped_audio_frames_before_start_count = 0;
                dropped_audio_frames_waiting_for_video_count = 0;
                video_decode_recovery
                    .reset_for_timeline_start(video_stream.codec_id, current_start_position_nsecs);
                buffered_reporter = BufferedReporter::new_with_events(
                    audio_output.is_some(),
                    emit_playback_buffered_events,
                );
                buffered_reporter.reset_to(position_seconds, session.id(), &event_tx);
                let _ = event_tx.send(BackendEvent::new(
                    session.id(),
                    BackendEventKind::PositionChanged(position_seconds),
                ));
                let _ = event_tx.send(BackendEvent::new(
                    session.id(),
                    BackendEventKind::SubtitleChanged(None),
                ));
                if pending_track_selection.pause_after_switch {
                    control.set_user_paused(true);
                    subtitle_pipeline.update_overlay(
                        current_start_position_nsecs,
                        session.id(),
                        &event_tx,
                    );
                    let _ = event_tx.send(BackendEvent::new(
                        session.id(),
                        BackendEventKind::Pause(true),
                    ));
                    let _ = event_tx.send(BackendEvent::new(
                        session.id(),
                        BackendEventKind::Buffering(false),
                    ));
                } else {
                    let _ = event_tx.send(BackendEvent::new(
                        session.id(),
                        BackendEventKind::Buffering(true),
                    ));
                }
                Ok(())
            })();
            if control.has_pending_seek() {
                packet.unref();
                continue;
            }
            switch_result?;
            continue;
        }

        if let Some(pending_seek) = drained_commands.pending_seek {
            let position_seconds = begin_seek(&mut session, &control, &pending_seek);
            current_start_position_nsecs = session.start_position_nsecs();
            let demux_seek_result =
                demux_cache.seek(position_seconds, session.id(), pending_seek.generation);
            tracing::debug!(
                session_id = ?session.id(),
                position_seconds,
                generation = pending_seek.generation,
                current_start_position_nsecs,
                ?demux_seek_result,
                "handling FFmpeg playback seek"
            );
            control.finish_seek(pending_seek.generation);
            flush_playback_decode_state(
                &video_decoder,
                audio_decoder.as_ref(),
                &mut subtitle_pipeline,
                &mut video_frame,
                audio_frame.as_mut(),
                &mut packet,
            );
            reset_playback_timeline_state(
                video_stream,
                audio_stream,
                video_frame_duration_nsecs,
                current_start_position_nsecs,
                &mut video_clock,
                &mut playback_timeline_origin_nsecs,
                &mut audio_clock,
                &mut scheduler,
                audio_output.as_ref(),
                &mut queued_video_frames,
                &mut first_video_frame_pending,
                &mut dovi_pipeline,
            );
            dropped_video_frames_before_start_count = 0;
            dropped_audio_frames_before_start_count = 0;
            dropped_audio_frames_waiting_for_video_count = 0;
            video_decode_recovery
                .reset_for_timeline_start(video_stream.codec_id, current_start_position_nsecs);
            subtitle_pipeline.reset_cues_for_position(current_start_position_nsecs);
            buffered_reporter = BufferedReporter::new_with_events(
                audio_output.is_some(),
                emit_playback_buffered_events,
            );
            buffered_reporter.reset_to(position_seconds, session.id(), &event_tx);
            let _ = event_tx.send(BackendEvent::new(
                session.id(),
                BackendEventKind::PositionChanged(position_seconds),
            ));
            let _ = event_tx.send(BackendEvent::new(
                session.id(),
                BackendEventKind::Buffering(true),
            ));
            let _ = event_tx.send(BackendEvent::new(
                session.id(),
                BackendEventKind::SubtitleChanged(None),
            ));
            if control.has_pending_seek() {
                packet.unref();
                continue;
            }
            continue;
        }

        if control.is_paused() {
            thread::sleep(SCHEDULER_POLL_INTERVAL);
            continue;
        }

        if control.has_pending_seek() {
            thread::yield_now();
            continue;
        }

        let video_output_waiting_for_demux =
            !first_video_frame_pending && queued_video_frames.is_empty();
        packet = match demux_cache.read_packet(video_output_waiting_for_demux) {
            DemuxReadResult::Packet(packet) => packet,
            DemuxReadResult::Eof => break,
            DemuxReadResult::Interrupted if control.should_stop() => break,
            DemuxReadResult::Interrupted => continue,
            DemuxReadResult::Error(error) => {
                if control.has_pending_seek() {
                    continue;
                }
                return Err(error);
            }
        };

        let process_result = match packet.stream_index() {
            index if index == video_decoder.stream_index => {
                video_packet_count = video_packet_count.saturating_add(1);
                if video_decode_recovery.should_skip_packet(&packet, video_stream.codec_id) {
                    let skipped_packets = video_decode_recovery.record_skipped_packet();
                    if skipped_packets == 1 || skipped_packets.is_multiple_of(60) {
                        tracing::debug!(
                            pts = ?packet.best_timestamp(),
                            keyframe = packet.is_key(),
                            codec = ?video_stream.codec_id,
                            packet_bytes = packet.byte_len(),
                            recovery_point = packet_is_video_recovery_point(&packet, video_stream.codec_id),
                            safe_seek_point = packet_is_video_seek_point(&packet, video_stream.codec_id),
                            skipped_packets,
                            "skipping FFmpeg video packets while waiting for decode recovery point"
                        );
                    }
                    Ok(())
                } else {
                    if video_decode_recovery.accept_recovery_point(&packet, video_stream.codec_id) {
                        tracing::debug!(
                            pts = ?packet.best_timestamp(),
                            keyframe = packet.is_key(),
                            codec = ?video_stream.codec_id,
                            packet_bytes = packet.byte_len(),
                            recovery_point = packet_is_video_recovery_point(&packet, video_stream.codec_id),
                            safe_seek_point = packet_is_video_seek_point(&packet, video_stream.codec_id),
                            "resuming FFmpeg video decode at recovery point"
                        );
                        video_decoder.flush_buffers();
                    } else if video_decode_recovery.accept_after_wait_limit(video_stream.codec_id) {
                        tracing::debug!(
                            pts = ?packet.best_timestamp(),
                            keyframe = packet.is_key(),
                            codec = ?video_stream.codec_id,
                            packet_bytes = packet.byte_len(),
                            recovery_point = packet_is_video_recovery_point(&packet, video_stream.codec_id),
                            safe_seek_point = packet_is_video_seek_point(&packet, video_stream.codec_id),
                            max_skipped_packets = VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS,
                            "resuming FFmpeg video decode after recovery point wait limit"
                        );
                        video_decoder.flush_buffers();
                    }
                    log_video_decode_packet_if_needed(
                        &packet,
                        video_stream.codec_id,
                        video_packet_count,
                        &video_decode_recovery,
                    );
                    let stripped_video_packet = strip_hevc_dovi_rpu_decode_packet(
                        &packet,
                        video_stream.codec_id,
                        HevcDecodePacketLogContext {
                            video_packet_count,
                            first_video_frame_pending,
                            recovery_waiting: video_decode_recovery.waiting_for_keyframe(),
                        },
                    )?;
                    if let Some(metadata) = stripped_video_packet
                        .as_ref()
                        .and_then(|packet| packet.metadata.clone())
                    {
                        tracing::trace!(
                            pts = ?packet.best_timestamp(),
                            profile = metadata.profile,
                            profile5 = metadata.is_profile5(),
                            rpu_bytes = metadata.rpu_payload.len(),
                            "using stripped Dolby Vision RPU metadata for FFmpeg packet"
                        );
                        dovi_pipeline.observe_video_packet_metadata(
                            &packet,
                            video_stream,
                            metadata,
                        );
                    } else {
                        dovi_pipeline.observe_video_packet(&packet, video_stream);
                    }
                    let stripped_to_empty = stripped_video_packet
                        .as_ref()
                        .is_some_and(|stripped| stripped.packet.byte_len() == 0);
                    let decode_packet_ptr = stripped_video_packet
                        .as_ref()
                        .map_or(packet.as_ptr(), |stripped| stripped.packet.as_ptr());
                    let decode_packet_for_recovery = stripped_video_packet
                        .as_ref()
                        .map_or(&packet, |stripped| &stripped.packet);
                    let decode_result = if stripped_to_empty {
                        Ok(())
                    } else {
                        video_decoder.decode_packet(
                            decode_packet_ptr,
                            &mut video_frame,
                            |frame| {
                            if control.has_pending_seek() {
                                return Ok(());
                            }
                            decoded_video_frame_count =
                                decoded_video_frame_count.saturating_add(1);
                            let frame_timestamp = frame_best_effort_timestamp(frame);
                            let timestamp = video_clock.map(frame_timestamp, video_decoder.time_base);
                            subtitle_pipeline.refresh_timeline_origin(
                                &mut playback_timeline_origin_nsecs,
                                &video_clock,
                            );
                            let frame_pts = FramePts {
                                nsecs: timestamp.timeline_nsecs,
                            };
                            if decoded_video_frame_count == 1
                                || decoded_video_frame_count.is_multiple_of(60)
                                || video_decode_recovery.waiting_for_keyframe()
                            {
                                tracing::debug!(
                                    frame_count = decoded_video_frame_count,
                                    raw_timestamp = frame_timestamp,
                                    timeline_nsecs = timestamp.timeline_nsecs,
                                    current_start_position_nsecs,
                                    first_video_frame_pending,
                                    recovery_waiting = video_decode_recovery.waiting_for_keyframe(),
                                    decode_error_flags = frame_decode_error_flags(frame),
                                    corrupt = frame_is_corrupt(frame),
                                    "decoded FFmpeg video frame"
                                );
                            }
                            if drop_corrupt_video_frame_if_needed(
                                frame,
                                frame_pts,
                                &mut dovi_pipeline,
                            ) {
                                return Err(CORRUPT_VIDEO_FRAME_RECOVERY_ERROR.to_string());
                            }
                            let realign_on_next_frame =
                                video_decode_recovery.take_realign_on_next_frame();
                            let start_action = decoded_video_frame_start_action(
                                timestamp.timeline_nsecs,
                                current_start_position_nsecs,
                                realign_on_next_frame,
                            );
                            match start_action {
                                DecodedVideoFrameStartAction::DropBeforeStart => {
                                    dropped_video_frames_before_start_count =
                                        dropped_video_frames_before_start_count.saturating_add(1);
                                    if dropped_video_frames_before_start_count == 1 {
                                        tracing::trace!(
                                            frame_count = decoded_video_frame_count,
                                            dropped_frames_before_start =
                                                dropped_video_frames_before_start_count,
                                            raw_timestamp = frame_timestamp,
                                            timeline_nsecs = timestamp.timeline_nsecs,
                                            current_start_position_nsecs,
                                            first_video_frame_pending,
                                            recovery_realign_on_next_frame =
                                                realign_on_next_frame,
                                            "dropping decoded FFmpeg video frame before playback start"
                                        );
                                    } else if dropped_video_frames_before_start_count
                                        .is_multiple_of(60)
                                    {
                                        tracing::debug!(
                                            frame_count = decoded_video_frame_count,
                                            dropped_frames_before_start =
                                                dropped_video_frames_before_start_count,
                                            raw_timestamp = frame_timestamp,
                                            timeline_nsecs = timestamp.timeline_nsecs,
                                            current_start_position_nsecs,
                                            first_video_frame_pending,
                                            recovery_realign_on_next_frame =
                                                realign_on_next_frame,
                                            "dropping decoded FFmpeg video frame before playback start"
                                        );
                                    }
                                    dovi_pipeline.discard_frame(frame_pts);
                                    return Ok(());
                                }
                                DecodedVideoFrameStartAction::Use { realign } if realign => {
                                    tracing::debug!(
                                        previous_start_position_nsecs =
                                            current_start_position_nsecs,
                                        pts = frame_pts.nsecs,
                                        "realigning FFmpeg playback clock to recovered video keyframe"
                                    );
                                    current_start_position_nsecs = frame_pts.nsecs;
                                    scheduler.reset(frame_pts.nsecs);
                                    if let Some(output) = audio_output.as_ref() {
                                        output.reset_clock(frame_pts.nsecs);
                                    }
                                    audio_clock = TimestampMapper::new(
                                        audio_stream.and_then(|stream| stream.start_nsecs),
                                        frame_pts.nsecs,
                                        None,
                                    );
                                    queued_video_frames.clear();
                                    first_video_frame_pending = true;
                                    subtitle_pipeline.reset_cues_for_position(frame_pts.nsecs);
                                    buffered_reporter.reset_to(
                                        nsecs_to_seconds(frame_pts.nsecs),
                                        session.id(),
                                        &event_tx,
                                    );
                                }
                                DecodedVideoFrameStartAction::Use { .. } => {}
                            }

                            if let Some(output) = audio_output.as_ref() {
                                if !first_video_frame_pending
                                    && should_drop_late_queued_video_frame(
                                        timestamp.timeline_nsecs,
                                        video_frame_duration_nsecs,
                                        output.played_timeline_nsecs(),
                                        &queued_video_frames,
                                    )
                                {
                                    tracing::trace!(
                                        pts = timestamp.timeline_nsecs,
                                        played_until = output.played_timeline_nsecs(),
                                        queued_frames = queued_video_frames.len(),
                                        "dropping late FFmpeg video frame while newer queued frames are available"
                                    );
                                    dovi_pipeline.discard_frame(frame_pts);
                                    return Ok(());
                                }
                                if should_drop_backlogged_vulkan_frame(
                                    frame,
                                    first_video_frame_pending,
                                    &frame_slot,
                                ) {
                                    dovi_pipeline.discard_frame(frame_pts);
                                    return Ok(());
                                }

                                let dovi_metadata =
                                    dovi_pipeline.metadata_for_decoded_frame(frame, frame_pts);
                                let mut decoded_frame = video_converter.convert(
                                    &video_decoder,
                                    frame,
                                    dovi_metadata,
                                )?;
                                decoded_frame.pts = Some(frame_pts);
                                subtitle_pipeline.update_overlay_from_audio_clock(
                                    output,
                                    session.id(),
                                    &event_tx,
                                );

                                if first_video_frame_pending {
                                    if timestamp.timeline_nsecs > current_start_position_nsecs {
                                        tracing::debug!(
                                            previous_start_position_nsecs =
                                                current_start_position_nsecs,
                                            first_video_frame_nsecs = timestamp.timeline_nsecs,
                                            "realigning FFmpeg playback start to first decoded video frame"
                                        );
                                        current_start_position_nsecs = timestamp.timeline_nsecs;
                                        scheduler.reset(timestamp.timeline_nsecs);
                                        output.reset_clock(timestamp.timeline_nsecs);
                                        subtitle_pipeline
                                            .reset_cues_for_position(timestamp.timeline_nsecs);
                                        buffered_reporter.reset_to(
                                            nsecs_to_seconds(timestamp.timeline_nsecs),
                                            session.id(),
                                            &event_tx,
                                        );
                                    }
                                    tracing::debug!(
                                        frame_count = decoded_video_frame_count,
                                        pts = timestamp.timeline_nsecs,
                                        current_start_position_nsecs,
                                        queued_video_frames = queued_video_frames.len(),
                                        audio_played_until_nsecs = output.played_timeline_nsecs(),
                                        "presenting first FFmpeg video frame after start gate"
                                    );
                                    present_decoded_video_frame(
                                        decoded_frame,
                                        session.id(),
                                        timestamp.timeline_nsecs,
                                        &frame_slot,
                                        &frame_presented,
                                        &mut position_reporter,
                                        &event_tx,
                                    );
                                    buffered_reporter.report_video_timeline_nsecs(
                                        timestamp
                                            .timeline_nsecs
                                            .saturating_add(video_frame_duration_nsecs),
                                        session.id(),
                                        &event_tx,
                                    );
                                    first_video_frame_pending = false;
                                    return Ok(());
                                }
                                queued_video_frames.push_back(QueuedVideoFrame {
                                    frame: decoded_frame,
                                    timeline_nsecs: timestamp.timeline_nsecs,
                                    duration_nsecs: video_frame_duration_nsecs,
                                });
                                buffered_reporter.report_video_timeline_nsecs(
                                    timestamp
                                        .timeline_nsecs
                                        .saturating_add(video_frame_duration_nsecs),
                                    session.id(),
                                    &event_tx,
                                );
                                let played_until = present_due_audio_clocked_video_frames(
                                    &mut queued_video_frames,
                                    output,
                                    session.id(),
                                    &frame_slot,
                                    &frame_presented,
                                    &mut position_reporter,
                                    &event_tx,
                                );
                                subtitle_pipeline.update_overlay(
                                    played_until,
                                    session.id(),
                                    &event_tx,
                                );
                                if queued_video_duration(&queued_video_frames)
                                    >= queued_video_limit_duration(
                                        &queued_video_frames,
                                        subtitle_pipeline.needs_prefetch(),
                                    )
                                {
                                    let target_duration = queued_video_target_duration(
                                        &queued_video_frames,
                                        subtitle_pipeline.needs_prefetch(),
                                    );
                                    wait_for_audio_clocked_video_queue(
                                        &mut queued_video_frames,
                                        output,
                                        &control,
                                        session.id(),
                                        &frame_slot,
                                        &frame_presented,
                                        &mut position_reporter,
                                        &event_tx,
                                        target_duration,
                                        |played_until| {
                                            subtitle_pipeline.update_overlay(
                                                played_until,
                                                session.id(),
                                                &event_tx,
                                            );
                                        },
                                    )?;
                                }
                            } else {
                                if should_drop_backlogged_vulkan_frame(
                                    frame,
                                    first_video_frame_pending,
                                    &frame_slot,
                                ) {
                                    dovi_pipeline.discard_frame(frame_pts);
                                    return Ok(());
                                }
                                let dovi_metadata =
                                    dovi_pipeline.metadata_for_decoded_frame(frame, frame_pts);
                                let mut decoded_frame = video_converter.convert(
                                    &video_decoder,
                                    frame,
                                    dovi_metadata,
                                )?;
                                decoded_frame.pts = Some(frame_pts);
                                subtitle_pipeline.update_overlay(
                                    timestamp.timeline_nsecs,
                                    session.id(),
                                    &event_tx,
                                );

                                if first_video_frame_pending {
                                    if timestamp.timeline_nsecs > current_start_position_nsecs {
                                        tracing::debug!(
                                            previous_start_position_nsecs =
                                                current_start_position_nsecs,
                                            first_video_frame_nsecs = timestamp.timeline_nsecs,
                                            "realigning FFmpeg playback start to first decoded video frame"
                                        );
                                        current_start_position_nsecs = timestamp.timeline_nsecs;
                                        scheduler.reset(timestamp.timeline_nsecs);
                                        subtitle_pipeline
                                            .reset_cues_for_position(timestamp.timeline_nsecs);
                                        buffered_reporter.reset_to(
                                            nsecs_to_seconds(timestamp.timeline_nsecs),
                                            session.id(),
                                            &event_tx,
                                        );
                                    }
                                    tracing::debug!(
                                        frame_count = decoded_video_frame_count,
                                        pts = timestamp.timeline_nsecs,
                                        current_start_position_nsecs,
                                        "presenting first FFmpeg video frame after start gate"
                                    );
                                    present_decoded_video_frame(
                                        decoded_frame,
                                        session.id(),
                                        timestamp.timeline_nsecs,
                                        &frame_slot,
                                        &frame_presented,
                                        &mut position_reporter,
                                        &event_tx,
                                    );
                                    buffered_reporter.report_video_timeline_nsecs(
                                        timestamp
                                            .timeline_nsecs
                                            .saturating_add(video_frame_duration_nsecs),
                                        session.id(),
                                        &event_tx,
                                    );
                                    first_video_frame_pending = false;
                                    return Ok(());
                                }
                                first_video_frame_pending = false;
                                if scheduler
                                    .wait_until(timestamp.timeline_nsecs, &control)
                                    .interrupted()
                                {
                                    return Ok(());
                                }
                                if control.has_pending_seek() {
                                    return Ok(());
                                }
                                present_decoded_video_frame(
                                    decoded_frame,
                                    session.id(),
                                    timestamp.timeline_nsecs,
                                    &frame_slot,
                                    &frame_presented,
                                    &mut position_reporter,
                                    &event_tx,
                                );
                                buffered_reporter.report_video_timeline_nsecs(
                                    timestamp
                                        .timeline_nsecs
                                        .saturating_add(video_frame_duration_nsecs),
                                    session.id(),
                                    &event_tx,
                                );
                            }
                            Ok(())
                            },
                        )
                    };
                    let realign_after_decode_recovery = first_video_frame_pending;
                    let process_result = recover_video_decode_error_if_needed(
                        decode_result,
                        &video_decoder,
                        video_stream.codec_id,
                        decode_packet_for_recovery,
                        &mut video_decode_recovery,
                        realign_after_decode_recovery,
                    );
                    if video_decode_recovery.waiting_for_keyframe() {
                        if realign_after_decode_recovery {
                            queued_video_frames.clear();
                            first_video_frame_pending = true;
                        }
                        dovi_pipeline.reset();
                    }
                    process_result
                }
            }
            index if subtitle_pipeline.matches_stream_index(index) => subtitle_pipeline
                .decode_packet(
                    &mut packet,
                    SubtitleDecodeContext {
                        current_start_position_nsecs,
                        playback_timeline_origin_nsecs,
                        control: &control,
                        audio_output: audio_output.as_ref(),
                        session_id: session.id(),
                        event_tx: &event_tx,
                    },
                ),
            index
                if audio_decoder
                    .as_ref()
                    .is_some_and(|decoder| index == decoder.stream_index) =>
            {
                let decoder = audio_decoder.as_ref().expect("audio decoder checked above");
                let frame = audio_frame
                    .as_mut()
                    .expect("audio frame exists with audio decoder");
                let resampler = audio_resampler
                    .as_mut()
                    .expect("audio resampler exists with audio decoder");
                let output = audio_output
                    .as_ref()
                    .expect("audio output exists with audio decoder");
                decoder.decode_packet(packet.as_ptr(), frame, |frame| {
                    if control.has_pending_seek() {
                        return Ok(());
                    }
                    let raw_timestamp = frame_best_effort_timestamp(frame);
                    let timestamp = audio_clock.map(raw_timestamp, decoder.time_base);
                    if timestamp.timeline_nsecs < current_start_position_nsecs {
                        dropped_audio_frames_before_start_count =
                            dropped_audio_frames_before_start_count.saturating_add(1);
                        if dropped_audio_frames_before_start_count == 1 {
                            tracing::trace!(
                                dropped_audio_frames_before_start =
                                    dropped_audio_frames_before_start_count,
                                raw_timestamp,
                                timeline_nsecs = timestamp.timeline_nsecs,
                                current_start_position_nsecs,
                                first_video_frame_pending,
                                "dropping FFmpeg audio frame before playback start"
                            );
                        } else if dropped_audio_frames_before_start_count.is_multiple_of(60) {
                            tracing::debug!(
                                dropped_audio_frames_before_start =
                                    dropped_audio_frames_before_start_count,
                                raw_timestamp,
                                timeline_nsecs = timestamp.timeline_nsecs,
                                current_start_position_nsecs,
                                first_video_frame_pending,
                                "dropping FFmpeg audio frame before playback start"
                            );
                        }
                        return Ok(());
                    }
                    if first_video_frame_pending {
                        dropped_audio_frames_waiting_for_video_count =
                            dropped_audio_frames_waiting_for_video_count.saturating_add(1);
                        if dropped_audio_frames_waiting_for_video_count == 1 {
                            tracing::trace!(
                                dropped_audio_frames_waiting_for_video =
                                    dropped_audio_frames_waiting_for_video_count,
                                raw_timestamp,
                                timeline_nsecs = timestamp.timeline_nsecs,
                                current_start_position_nsecs,
                                queued_video_frames = queued_video_frames.len(),
                                "dropping FFmpeg audio frame while waiting for first video frame"
                            );
                        } else if dropped_audio_frames_waiting_for_video_count.is_multiple_of(60) {
                            tracing::debug!(
                                dropped_audio_frames_waiting_for_video =
                                    dropped_audio_frames_waiting_for_video_count,
                                raw_timestamp,
                                timeline_nsecs = timestamp.timeline_nsecs,
                                current_start_position_nsecs,
                                queued_video_frames = queued_video_frames.len(),
                                "dropping FFmpeg audio frame while waiting for first video frame"
                            );
                        }
                        return Ok(());
                    }
                    if let Some(audio) = resampler.convert(frame)? {
                        if control.has_pending_seek() {
                            return Ok(());
                        }
                        let buffered_until_nsecs = timestamp
                            .timeline_nsecs
                            .saturating_add(audio.duration_nsecs);
                        output.push_timed(
                            audio.samples,
                            timestamp.timeline_nsecs,
                            buffered_until_nsecs,
                            &control,
                            || {
                                let played_until = present_due_audio_clocked_video_frames(
                                    &mut queued_video_frames,
                                    output,
                                    session.id(),
                                    &frame_slot,
                                    &frame_presented,
                                    &mut position_reporter,
                                    &event_tx,
                                );
                                subtitle_pipeline.update_overlay(
                                    played_until,
                                    session.id(),
                                    &event_tx,
                                );
                                Ok(())
                            },
                        )?;
                        buffered_reporter.report_audio_timeline_nsecs(
                            buffered_until_nsecs,
                            session.id(),
                            &event_tx,
                        );
                    }
                    Ok(())
                })
            }
            _ => Ok(()),
        };
        packet.unref();
        if let Err(error) = process_result {
            if control.has_pending_seek() {
                continue;
            }
            return Err(error);
        }
        if control.has_pending_seek() {
            continue;
        }
    }

    if control.should_stop() {
        return Ok(());
    }

    video_decoder.flush(&mut video_frame, |frame| {
        let timestamp =
            video_clock.map(frame_best_effort_timestamp(frame), video_decoder.time_base);
        subtitle_pipeline
            .refresh_timeline_origin(&mut playback_timeline_origin_nsecs, &video_clock);
        if timestamp.timeline_nsecs < current_start_position_nsecs {
            return Ok(());
        }
        let frame_pts = FramePts {
            nsecs: timestamp.timeline_nsecs,
        };
        if drop_corrupt_video_frame_if_needed(frame, frame_pts, &mut dovi_pipeline) {
            return Ok(());
        }
        if let Some(output) = audio_output.as_ref() {
            if !first_video_frame_pending
                && should_drop_late_queued_video_frame(
                    timestamp.timeline_nsecs,
                    video_frame_duration_nsecs,
                    output.played_timeline_nsecs(),
                    &queued_video_frames,
                )
            {
                tracing::trace!(
                    pts = timestamp.timeline_nsecs,
                    played_until = output.played_timeline_nsecs(),
                    queued_frames = queued_video_frames.len(),
                    "dropping late FFmpeg video frame while newer queued frames are available"
                );
                dovi_pipeline.discard_frame(frame_pts);
                return Ok(());
            }
            if should_drop_backlogged_vulkan_frame(frame, first_video_frame_pending, &frame_slot) {
                dovi_pipeline.discard_frame(frame_pts);
                return Ok(());
            }

            let dovi_metadata = dovi_pipeline.metadata_for_decoded_frame(frame, frame_pts);
            let mut decoded_frame =
                video_converter.convert(&video_decoder, frame, dovi_metadata)?;
            decoded_frame.pts = Some(frame_pts);
            subtitle_pipeline.update_overlay_from_audio_clock(output, session.id(), &event_tx);

            if first_video_frame_pending {
                present_decoded_video_frame(
                    decoded_frame,
                    session.id(),
                    timestamp.timeline_nsecs,
                    &frame_slot,
                    &frame_presented,
                    &mut position_reporter,
                    &event_tx,
                );
                buffered_reporter.report_video_timeline_nsecs(
                    timestamp
                        .timeline_nsecs
                        .saturating_add(video_frame_duration_nsecs),
                    session.id(),
                    &event_tx,
                );
                first_video_frame_pending = false;
                return Ok(());
            }
            queued_video_frames.push_back(QueuedVideoFrame {
                frame: decoded_frame,
                timeline_nsecs: timestamp.timeline_nsecs,
                duration_nsecs: video_frame_duration_nsecs,
            });
            buffered_reporter.report_video_timeline_nsecs(
                timestamp
                    .timeline_nsecs
                    .saturating_add(video_frame_duration_nsecs),
                session.id(),
                &event_tx,
            );
            let played_until = present_due_audio_clocked_video_frames(
                &mut queued_video_frames,
                output,
                session.id(),
                &frame_slot,
                &frame_presented,
                &mut position_reporter,
                &event_tx,
            );
            subtitle_pipeline.update_overlay(played_until, session.id(), &event_tx);
        } else {
            if should_drop_backlogged_vulkan_frame(frame, first_video_frame_pending, &frame_slot) {
                dovi_pipeline.discard_frame(frame_pts);
                return Ok(());
            }
            let dovi_metadata = dovi_pipeline.metadata_for_decoded_frame(frame, frame_pts);
            let mut decoded_frame =
                video_converter.convert(&video_decoder, frame, dovi_metadata)?;
            decoded_frame.pts = Some(frame_pts);
            subtitle_pipeline.update_overlay(timestamp.timeline_nsecs, session.id(), &event_tx);

            first_video_frame_pending = false;
            if scheduler
                .wait_until(timestamp.timeline_nsecs, &control)
                .interrupted()
            {
                return Ok(());
            }
            present_decoded_video_frame(
                decoded_frame,
                session.id(),
                timestamp.timeline_nsecs,
                &frame_slot,
                &frame_presented,
                &mut position_reporter,
                &event_tx,
            );
            buffered_reporter.report_video_timeline_nsecs(
                timestamp
                    .timeline_nsecs
                    .saturating_add(video_frame_duration_nsecs),
                session.id(),
                &event_tx,
            );
        }
        Ok(())
    })?;

    if let (Some(decoder), Some(frame), Some(resampler), Some(output)) = (
        audio_decoder.as_ref(),
        audio_frame.as_mut(),
        audio_resampler.as_mut(),
        audio_output.as_ref(),
    ) {
        decoder.flush(frame, |frame| {
            let timestamp = audio_clock.map(frame_best_effort_timestamp(frame), decoder.time_base);
            if timestamp.timeline_nsecs < current_start_position_nsecs {
                return Ok(());
            }
            if let Some(audio) = resampler.convert(frame)? {
                let buffered_until_nsecs = timestamp
                    .timeline_nsecs
                    .saturating_add(audio.duration_nsecs);
                output.push_timed(
                    audio.samples,
                    timestamp.timeline_nsecs,
                    buffered_until_nsecs,
                    &control,
                    || {
                        let played_until = present_due_audio_clocked_video_frames(
                            &mut queued_video_frames,
                            output,
                            session.id(),
                            &frame_slot,
                            &frame_presented,
                            &mut position_reporter,
                            &event_tx,
                        );
                        subtitle_pipeline.update_overlay(played_until, session.id(), &event_tx);
                        Ok(())
                    },
                )?;
                buffered_reporter.report_audio_timeline_nsecs(
                    buffered_until_nsecs,
                    session.id(),
                    &event_tx,
                );
            }
            Ok(())
        })?;
    }

    buffered_reporter.report_value(duration_seconds, session.id(), &event_tx);
    if let Some(output) = &audio_output {
        drain_audio_clocked_video_queue(
            &mut queued_video_frames,
            output,
            &control,
            session.id(),
            &frame_slot,
            &frame_presented,
            &mut position_reporter,
            &event_tx,
            |played_until| {
                subtitle_pipeline.update_overlay(played_until, session.id(), &event_tx);
            },
        )?;
        output.drain(&control)?;
    }
    Ok(())
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

fn drop_corrupt_video_frame_if_needed(
    frame: *mut ffi::AVFrame,
    frame_pts: FramePts,
    dovi_pipeline: &mut DoviPipeline,
) -> bool {
    if !frame_is_corrupt(frame) {
        return false;
    }

    tracing::debug!(
        pts = frame_pts.nsecs,
        decode_error_flags = frame_decode_error_flags(frame),
        "dropping corrupt FFmpeg video frame"
    );
    dovi_pipeline.discard_frame(frame_pts);
    true
}

fn strip_hevc_dovi_rpu_decode_packet(
    packet: &AvPacket,
    codec_id: ffi::AVCodecID,
    log_context: HevcDecodePacketLogContext,
) -> std::result::Result<Option<StrippedDoviDecodePacket>, String> {
    if codec_id != ffi::AVCodecID::AV_CODEC_ID_HEVC {
        return Ok(None);
    }
    let Some(data) = packet.data() else {
        return Ok(None);
    };
    let Some(stripped) = strip_dovi_rpu_nalus(data) else {
        if should_debug_hevc_decode_packet_without_rpu(log_context) {
            tracing::debug!(
                packet_count = log_context.video_packet_count,
                pts = ?packet.best_timestamp(),
                keyframe = packet.is_key(),
                packet_bytes = packet.byte_len(),
                first_video_frame_pending = log_context.first_video_frame_pending,
                recovery_waiting = log_context.recovery_waiting,
                original_nals = %hevc_nal_summary(data, None),
                "HEVC decode packet has no stripped Dolby Vision RPU NALs"
            );
        } else if should_trace_hevc_decode_packet_nals(packet, log_context) {
            tracing::trace!(
                packet_count = log_context.video_packet_count,
                pts = ?packet.best_timestamp(),
                keyframe = packet.is_key(),
                packet_bytes = packet.byte_len(),
                first_video_frame_pending = log_context.first_video_frame_pending,
                recovery_waiting = log_context.recovery_waiting,
                original_nals = %hevc_nal_summary(data, None),
                "HEVC decode packet has no stripped Dolby Vision RPU NALs"
            );
        }
        return Ok(None);
    };

    if should_debug_stripped_hevc_dovi_packet(log_context, &stripped) {
        tracing::debug!(
            packet_count = log_context.video_packet_count,
            pts = ?packet.best_timestamp(),
            keyframe = packet.is_key(),
            packet_bytes = packet.byte_len(),
            stripped_packet_bytes = stripped.data.len(),
            stripped_bytes = stripped.stripped_bytes,
            nal_count = stripped.nal_count,
            stripped_nal_count = stripped.stripped_nal_count,
            stream_format = ?stripped.stream_format,
            rpu_metadata = stripped.metadata.is_some(),
            rpu_profile = ?stripped.metadata.as_ref().map(|metadata| metadata.profile),
            rpu_profile5 = ?stripped.metadata.as_ref().map(DoviFrameMetadata::is_profile5),
            first_video_frame_pending = log_context.first_video_frame_pending,
            recovery_waiting = log_context.recovery_waiting,
            original_nals = %hevc_nal_summary(data, Some(stripped.stream_format)),
            stripped_nals = %hevc_nal_summary(&stripped.data, Some(stripped.stream_format)),
            "stripped Dolby Vision RPU NALs before HEVC decode"
        );
    } else if should_trace_hevc_decode_packet_nals(packet, log_context) {
        tracing::trace!(
            packet_count = log_context.video_packet_count,
            pts = ?packet.best_timestamp(),
            keyframe = packet.is_key(),
            packet_bytes = packet.byte_len(),
            stripped_packet_bytes = stripped.data.len(),
            stripped_bytes = stripped.stripped_bytes,
            nal_count = stripped.nal_count,
            stripped_nal_count = stripped.stripped_nal_count,
            stream_format = ?stripped.stream_format,
            rpu_metadata = stripped.metadata.is_some(),
            rpu_profile = ?stripped.metadata.as_ref().map(|metadata| metadata.profile),
            rpu_profile5 = ?stripped.metadata.as_ref().map(DoviFrameMetadata::is_profile5),
            original_nals = %hevc_nal_summary(data, Some(stripped.stream_format)),
            stripped_nals = %hevc_nal_summary(&stripped.data, Some(stripped.stream_format)),
            "stripped Dolby Vision RPU NALs before HEVC decode"
        );
    }

    AvPacket::from_data_and_props(&stripped.data, packet).map(|packet| {
        Some(StrippedDoviDecodePacket {
            packet,
            metadata: stripped.metadata,
        })
    })
}

struct StrippedDoviDecodePacket {
    packet: AvPacket,
    metadata: Option<DoviFrameMetadata>,
}

#[derive(Clone, Copy)]
struct HevcDecodePacketLogContext {
    video_packet_count: u64,
    first_video_frame_pending: bool,
    recovery_waiting: bool,
}

fn should_debug_hevc_decode_packet_without_rpu(context: HevcDecodePacketLogContext) -> bool {
    context.recovery_waiting
}

fn should_debug_stripped_hevc_dovi_packet(
    context: HevcDecodePacketLogContext,
    stripped: &DoviRpuStripResult,
) -> bool {
    context.recovery_waiting || stripped.metadata.is_none()
}

fn should_trace_hevc_decode_packet_nals(
    packet: &AvPacket,
    context: HevcDecodePacketLogContext,
) -> bool {
    context.first_video_frame_pending
        || context.recovery_waiting
        || packet.is_key()
        || context.video_packet_count == 1
        || context.video_packet_count.is_multiple_of(120)
}

fn hevc_nal_summary(data: &[u8], format_hint: Option<HevcStreamFormat>) -> String {
    let format = format_hint.or_else(|| detect_hevc_stream_format(data));
    match format {
        Some(HevcStreamFormat::ByteStream) => hevc_annex_b_nal_summary(data),
        Some(HevcStreamFormat::LengthPrefixed { length_size }) => {
            hevc_length_prefixed_nal_summary(data, length_size)
        }
        None => format!("format=unknown;bytes={}", data.len()),
    }
}

fn detect_hevc_stream_format(data: &[u8]) -> Option<HevcStreamFormat> {
    if data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1]) {
        return Some(HevcStreamFormat::ByteStream);
    }
    for length_size in [4, 3, 2, 1] {
        if hevc_length_prefixed_nal_types(data, length_size).is_some() {
            return Some(HevcStreamFormat::LengthPrefixed { length_size });
        }
    }
    if data.windows(3).any(|window| window == [0, 0, 1])
        || data.windows(4).any(|window| window == [0, 0, 0, 1])
    {
        return Some(HevcStreamFormat::ByteStream);
    }
    None
}

fn hevc_length_prefixed_nal_types(
    data: &[u8],
    length_size: usize,
) -> Option<Vec<(Option<u8>, usize)>> {
    let mut offset = 0usize;
    let mut nals = Vec::new();
    while offset < data.len() {
        let length_end = offset.checked_add(length_size)?;
        if length_end > data.len() {
            return None;
        }
        let mut nal_len = 0usize;
        for byte in &data[offset..length_end] {
            nal_len = nal_len.checked_shl(8)?.checked_add(usize::from(*byte))?;
        }
        if nal_len == 0 {
            return None;
        }
        let nal_start = length_end;
        let nal_end = nal_start.checked_add(nal_len)?;
        if nal_end > data.len() {
            return None;
        }
        let nal = trim_hevc_nal_trailing_zeroes(&data[nal_start..nal_end]);
        nals.push((nal.first().map(|header| (header >> 1) & 0x3f), nal.len()));
        offset = nal_end;
    }
    Some(nals)
}

fn hevc_length_prefixed_nal_summary(data: &[u8], length_size: usize) -> String {
    match hevc_length_prefixed_nal_types(data, length_size) {
        Some(nals) => format_hevc_nal_summary(
            format!("length_prefixed({length_size})"),
            data.len(),
            &nals,
            None,
        ),
        None => format!(
            "format=length_prefixed({length_size});bytes={};parse_error=true",
            data.len()
        ),
    }
}

fn hevc_annex_b_nal_summary(data: &[u8]) -> String {
    let mut cursor = 0usize;
    let mut nals = Vec::new();
    while let Some((start_code_pos, start_code_len)) = find_hevc_start_code(data, cursor) {
        let nal_start = start_code_pos.saturating_add(start_code_len);
        let nal_end = find_hevc_start_code(data, nal_start)
            .map(|(next_start, _)| next_start)
            .unwrap_or(data.len());
        let nal = trim_hevc_nal_trailing_zeroes(&data[nal_start..nal_end]);
        if !nal.is_empty() {
            nals.push((nal.first().map(|header| (header >> 1) & 0x3f), nal.len()));
        }
        cursor = nal_end;
    }
    let parse_error = nals.is_empty().then_some("no_start_code_nals");
    format_hevc_nal_summary("annex_b".to_string(), data.len(), &nals, parse_error)
}

fn format_hevc_nal_summary(
    format: String,
    bytes: usize,
    nals: &[(Option<u8>, usize)],
    parse_error: Option<&'static str>,
) -> String {
    const NAL_SUMMARY_LIMIT: usize = 16;
    let rpu_nals = nals
        .iter()
        .filter(|(nal_type, _)| *nal_type == Some(62))
        .count();
    let nal_parts = nals
        .iter()
        .take(NAL_SUMMARY_LIMIT)
        .enumerate()
        .map(|(index, (nal_type, len))| format!("{index}:{nal_type:?}/{len}"))
        .collect::<Vec<_>>()
        .join(",");
    let truncated = if nals.len() > NAL_SUMMARY_LIMIT {
        ";truncated=true"
    } else {
        ""
    };
    let parse_error = parse_error
        .map(|error| format!(";parse_error={error}"))
        .unwrap_or_default();
    format!(
        "format={format};bytes={bytes};count={};rpu62={rpu_nals};nals=[{nal_parts}]{truncated}{parse_error}",
        nals.len()
    )
}

fn find_hevc_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut index = from;
    while index + 3 <= data.len() {
        if data[index..].starts_with(&[0, 0, 1]) {
            return Some((index, 3));
        }
        if data[index..].starts_with(&[0, 0, 0, 1]) {
            return Some((index, 4));
        }
        index = index.saturating_add(1);
    }
    None
}

fn trim_hevc_nal_trailing_zeroes(nal: &[u8]) -> &[u8] {
    let mut end = nal.len();
    while end > 0 && nal[end - 1] == 0 {
        end -= 1;
    }
    &nal[..end]
}

fn log_video_decode_packet_if_needed(
    packet: &AvPacket,
    codec_id: ffi::AVCodecID,
    video_packet_count: u64,
    recovery: &VideoDecodeRecovery,
) {
    let recovery_point = packet_is_video_recovery_point(packet, codec_id);
    let safe_seek_point = packet_is_video_seek_point(packet, codec_id);
    if video_packet_count != 1
        && !video_packet_count.is_multiple_of(120)
        && !recovery.waiting_for_keyframe()
        && !packet.is_key()
        && !recovery_point
        && !safe_seek_point
    {
        return;
    }

    tracing::debug!(
        packet_count = video_packet_count,
        pts = ?packet.best_timestamp(),
        keyframe = packet.is_key(),
        codec = ?codec_id,
        packet_bytes = packet.byte_len(),
        recovery_point,
        safe_seek_point,
        recovery_waiting = recovery.waiting_for_keyframe(),
        recovery_skipped_packets = recovery.skipped_packets,
        "decoding FFmpeg video packet"
    );
}

fn recover_video_decode_error_if_needed(
    result: std::result::Result<(), String>,
    video_decoder: &Decoder,
    codec_id: ffi::AVCodecID,
    packet: &AvPacket,
    recovery: &mut VideoDecodeRecovery,
    realign_after_recovery_point: bool,
) -> std::result::Result<(), String> {
    match result {
        Ok(()) => Ok(()),
        Err(error) if video_decode_error_is_recoverable(&error) => {
            tracing::debug!(
                %error,
                codec = ?codec_id,
                packet_pts = ?packet.best_timestamp(),
                packet_keyframe = packet.is_key(),
                packet_bytes = packet.byte_len(),
                recovery_point = packet_is_video_recovery_point(packet, codec_id),
                safe_seek_point = packet_is_video_seek_point(packet, codec_id),
                recovery_waiting_before = recovery.waiting_for_keyframe(),
                recovery_skipped_packets = recovery.skipped_packets,
                realign_after_recovery_point,
                "recovering FFmpeg video decoder after damaged reference chain"
            );
            video_decoder.flush_buffers();
            recovery.begin_with_realign(realign_after_recovery_point);
            Ok(())
        }
        Err(error) => Err(error),
    }
}

pub(super) fn video_decode_error_is_recoverable(error: &str) -> bool {
    !video_decode_error_is_resource_exhaustion(error)
        && (error == CORRUPT_VIDEO_FRAME_RECOVERY_ERROR
            || error.starts_with("FFmpeg 发送解码包失败")
            || error.starts_with("FFmpeg 接收解码帧失败"))
}

fn video_decode_error_is_resource_exhaustion(error: &str) -> bool {
    error.contains("Cannot allocate memory") || error.contains("VK_ERROR_OUT_OF_DEVICE_MEMORY")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DecodedVideoFrameStartAction {
    DropBeforeStart,
    Use { realign: bool },
}

pub(super) fn decoded_video_frame_start_action(
    frame_timeline_nsecs: u64,
    current_start_position_nsecs: u64,
    recovery_realign: bool,
) -> DecodedVideoFrameStartAction {
    if recovery_realign {
        return DecodedVideoFrameStartAction::Use { realign: true };
    }
    if frame_timeline_nsecs < current_start_position_nsecs {
        return DecodedVideoFrameStartAction::DropBeforeStart;
    }
    DecodedVideoFrameStartAction::Use { realign: false }
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

fn decode_subtitle_packet_into_queue(
    decoder: &Decoder,
    stream: StreamInfo,
    packet: &AvPacket,
    current_start_position_nsecs: u64,
    playback_timeline_origin_nsecs: Option<u64>,
    subtitle_cues: &mut VecDeque<BackendSubtitleCue>,
    control: &FfmpegControl,
) -> std::result::Result<(), String> {
    let packet_timestamp = packet.best_timestamp();
    if stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_SUBRIP
        && let Some(cue) = packet.data().and_then(|data| {
            decoded_subrip_packet_cue(
                data,
                packet
                    .duration()
                    .and_then(|duration| timestamp_to_nsecs(duration, stream.time_base)),
            )
        })
    {
        if control.has_pending_seek() {
            return Ok(());
        }
        queue_decoded_subtitle_cue(
            cue,
            packet_timestamp,
            stream,
            current_start_position_nsecs,
            playback_timeline_origin_nsecs,
            subtitle_cues,
        );
        return Ok(());
    }

    decoder.decode_subtitle_packet(packet.as_ptr(), |cue| {
        if control.has_pending_seek() {
            return Ok(());
        }
        queue_decoded_subtitle_cue(
            cue,
            packet_timestamp,
            stream,
            current_start_position_nsecs,
            playback_timeline_origin_nsecs,
            subtitle_cues,
        );
        Ok(())
    })
}

fn queue_decoded_subtitle_cue(
    cue: DecodedSubtitleCue,
    packet_timestamp: Option<i64>,
    stream: StreamInfo,
    current_start_position_nsecs: u64,
    playback_timeline_origin_nsecs: Option<u64>,
    subtitle_cues: &mut VecDeque<BackendSubtitleCue>,
) {
    let Some(base_timeline_nsecs) = subtitle_cue_timeline_nsecs(
        cue.pts_nsecs,
        packet_timestamp,
        stream,
        playback_timeline_origin_nsecs,
    ) else {
        tracing::debug!(
            stream_index = stream.index,
            ?packet_timestamp,
            cue_pts_nsecs = ?cue.pts_nsecs,
            "dropping decoded subtitle cue without timestamp"
        );
        return;
    };
    let cue_has_content = cue.has_content();
    let subtitle_cue = BackendSubtitleCue {
        text: cue.text,
        bitmaps: cue.bitmaps,
        start_nsecs: base_timeline_nsecs.saturating_add(cue.start_offset_nsecs),
        end_nsecs: base_timeline_nsecs.saturating_add(cue.end_offset_nsecs),
    };
    if !cue_has_content {
        if stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE {
            trim_overlapping_subtitle_cues_at(subtitle_cues, subtitle_cue.start_nsecs);
        }
        return;
    }
    if subtitle_cue.end_nsecs >= current_start_position_nsecs {
        if stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE {
            trim_overlapping_subtitle_cues_at(subtitle_cues, subtitle_cue.start_nsecs);
        }
        push_subtitle_cue(subtitle_cues, subtitle_cue);
    }
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
