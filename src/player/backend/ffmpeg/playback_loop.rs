use super::*;

mod commands;
mod decode;
mod session;
mod subtitles;
mod timeline;

use commands::{begin_seek, begin_track_switch};
use decode::{flush_playback_decode_state, should_drop_backlogged_vulkan_frame};
use session::PlaybackSession;
use subtitles::{SubtitleDecodeContext, SubtitlePipeline};
use timeline::reset_playback_timeline_state;

const END_OF_PLAYBACK_READ_ERROR_TOLERANCE_SECONDS: f64 = 2.0;

struct OpenedPlaybackInput {
    input: FormatContext,
    video_stream: StreamInfo,
    video_decoder: Decoder,
    audio_stream: Option<StreamInfo>,
    audio_decoder: Option<Decoder>,
    subtitle_stream: Option<StreamInfo>,
    subtitle_decoder: Option<Decoder>,
}

struct ProbedPlaybackInput {
    input: FormatContext,
    video_stream: StreamInfo,
    audio_stream: Option<StreamInfo>,
    subtitle_stream: Option<StreamInfo>,
    allow_audio_decoder_failure: bool,
}

fn open_playback_input_with_fallback(
    source: &FfmpegPlaybackInput,
    control: Arc<FfmpegControl>,
    event_tx: &Sender<BackendEvent>,
) -> std::result::Result<OpenedPlaybackInput, String> {
    let initial_probe_profile = initial_probe_profile(source);
    let probed = match probe_playback_input(
        source,
        Arc::clone(&control),
        event_tx,
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
            match probe_playback_input(source, control, event_tx, fallback_probe_profile, true) {
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
            probe_playback_input(source, control, event_tx, fallback_probe_profile, true).map_err(
                |fallback_error| {
                    format!(
                        "FFmpeg 初始探测失败：{initial_error}；重试探测也失败：{fallback_error}"
                    )
                },
            )?
        }
    };
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
    control: Arc<FfmpegControl>,
    event_tx: &Sender<BackendEvent>,
    probe_profile: InputProbeProfile,
    allow_audio_decoder_failure: bool,
) -> std::result::Result<ProbedPlaybackInput, String> {
    let mut input = FormatContext::open(
        &source.url,
        source.http_headers.as_slice(),
        source.content_length,
        probe_profile,
        Arc::clone(&control),
        event_tx.clone(),
    )?;
    input.find_stream_info()?;

    let video_stream = input
        .best_stream(ffi::AVMediaType::AVMEDIA_TYPE_VIDEO)?
        .ok_or_else(|| "FFmpeg 未找到可解码视频流".to_string())?;
    let audio_stream = select_audio_stream(source, &input, allow_audio_decoder_failure)?;
    let subtitle_stream = select_subtitle_stream(source, &input)?;

    Ok(ProbedPlaybackInput {
        input,
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
        input.seek_stream(video_stream, source.start_position_seconds)?;
    }
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
    let mut audio_clock = TimestampMapper::new(
        audio_stream.and_then(|stream| stream.start_nsecs),
        current_start_position_nsecs,
        None,
    );
    if let Some(output) = &audio_output {
        output.reset_clock(current_start_position_nsecs);
    }

    if let Some(duration) = input.duration_seconds() {
        let _ = event_tx.send(BackendEvent::new(
            session.id(),
            BackendEventKind::DurationChanged(duration),
        ));
    }
    let _ = event_tx.send(BackendEvent::new(
        session.id(),
        BackendEventKind::PlaybackInfoChanged(playback_video_info(video_stream, &video_decoder)),
    ));
    let duration_seconds = input.duration_seconds();

    let mut packet = AvPacket::new()?;
    let mut buffered_reporter = BufferedReporter::new(audio_output.is_some());
    let mut queued_video_frames = VecDeque::new();
    let mut first_video_frame_pending = true;
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
        if control.wait_while_paused() {
            continue;
        }

        let drained_commands = drain_playback_commands(&command_rx, &control);
        if control.should_stop() {
            break;
        }

        if let Some(pending_track_selection) = drained_commands.pending_track_selection {
            let position_seconds =
                begin_track_switch(&mut session, &control, &pending_track_selection);
            let switch_result: std::result::Result<(), String> = (|| {
                current_start_position_nsecs = session.start_position_nsecs();
                input.seek_stream(video_stream, position_seconds)?;
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
                audio_stream =
                    select_audio_stream_for_selection(&source.selected_tracks, &input, false)?;
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

                subtitle_pipeline.switch_tracks(
                    &source,
                    &input,
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
                buffered_reporter = BufferedReporter::new(audio_output.is_some());
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
                    control.set_paused(true);
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
            let seek_result: std::result::Result<(), String> = (|| {
                current_start_position_nsecs = session.start_position_nsecs();
                input.seek_stream(video_stream, position_seconds)?;
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
                subtitle_pipeline.reset_cues_for_position(current_start_position_nsecs);
                buffered_reporter = BufferedReporter::new(audio_output.is_some());
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
                Ok(())
            })();
            if control.has_pending_seek() {
                packet.unref();
                continue;
            }
            seek_result?;
            continue;
        }

        if control.has_pending_seek() {
            thread::yield_now();
            continue;
        }

        let read = unsafe { ffi::av_read_frame(input.as_mut_ptr(), packet.as_mut_ptr()) };
        if playback_read_finished(read, duration_seconds, buffered_reporter.buffered_until()) {
            break;
        }
        if read < 0 {
            if control.has_pending_seek() {
                packet.unref();
                continue;
            }
            return Err(format!("FFmpeg 读取媒体包失败：{}", ffmpeg_error(read)));
        }

        let process_result = match packet.stream_index() {
            index if index == video_decoder.stream_index => {
                dovi_pipeline.observe_video_packet(&packet, video_stream);
                video_decoder.decode_packet(packet.as_ptr(), &mut video_frame, |frame| {
                    if control.has_pending_seek() {
                        return Ok(());
                    }
                    let timestamp = video_clock
                        .map(frame_best_effort_timestamp(frame), video_decoder.time_base);
                    subtitle_pipeline
                        .refresh_timeline_origin(&mut playback_timeline_origin_nsecs, &video_clock);
                    let frame_pts = FramePts {
                        nsecs: timestamp.timeline_nsecs,
                    };
                    if timestamp.timeline_nsecs < current_start_position_nsecs {
                        dovi_pipeline.discard_frame(frame_pts);
                        return Ok(());
                    }

                    if let Some(output) = audio_output.as_ref() {
                        if !first_video_frame_pending
                            && should_drop_late_video_frame(
                                timestamp.timeline_nsecs,
                                video_frame_duration_nsecs,
                                output.played_timeline_nsecs(),
                            )
                        {
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
                        let mut decoded_frame =
                            video_converter.convert(&video_decoder, frame, dovi_metadata)?;
                        decoded_frame.pts = Some(frame_pts);
                        subtitle_pipeline.update_overlay_from_audio_clock(
                            output,
                            session.id(),
                            &event_tx,
                        );

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
                        let mut decoded_frame =
                            video_converter.convert(&video_decoder, frame, dovi_metadata)?;
                        decoded_frame.pts = Some(frame_pts);
                        subtitle_pipeline.update_overlay(
                            timestamp.timeline_nsecs,
                            session.id(),
                            &event_tx,
                        );

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
                })
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
                    let timestamp =
                        audio_clock.map(frame_best_effort_timestamp(frame), decoder.time_base);
                    if timestamp.timeline_nsecs < current_start_position_nsecs {
                        return Ok(());
                    }
                    if let Some(audio) = resampler.convert(frame)? {
                        if control.has_pending_seek() {
                            return Ok(());
                        }
                        let buffered_until_nsecs = timestamp
                            .timeline_nsecs
                            .saturating_add(audio.duration_nsecs);
                        output.push(audio.samples, &control, || {
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
                        })?;
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
        if let Some(output) = audio_output.as_ref() {
            if !first_video_frame_pending
                && should_drop_late_video_frame(
                    timestamp.timeline_nsecs,
                    video_frame_duration_nsecs,
                    output.played_timeline_nsecs(),
                )
            {
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
                output.push(audio.samples, &control, || {
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
                })?;
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
