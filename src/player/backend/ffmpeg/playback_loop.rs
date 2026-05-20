use super::*;

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
    let Some(stream_index) = source.selected_tracks.audio_stream_index else {
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
    if source.selected_tracks.subtitle_external_url.is_some() {
        return Ok(None);
    }
    let Some(stream_index) = source.selected_tracks.subtitle_stream_index else {
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
    source: FfmpegPlaybackInput,
    frame_slot: FrameSlot,
    event_tx: Sender<BackendEvent>,
    control: Arc<FfmpegControl>,
    command_rx: Receiver<FfmpegCommand>,
    frame_presented: Arc<AtomicBool>,
) -> std::result::Result<(), String> {
    let mut current_session_id = source.session_id;
    control.set_session_id(current_session_id);
    let OpenedPlaybackInput {
        mut input,
        video_stream,
        video_decoder,
        audio_stream,
        audio_decoder: opened_audio_decoder,
        subtitle_stream,
        subtitle_decoder,
    } = open_playback_input_with_fallback(&source, Arc::clone(&control), &event_tx)?;
    if let Some(device) = video_decoder.vulkan_device() {
        frame_slot.request_vulkan_prewarm(current_session_id, device);
    }
    if source.start_position_seconds > 0.0 {
        input.seek_stream(video_stream, source.start_position_seconds)?;
    }
    let mut video_frame = AvFrame::new()?;
    let mut video_converter = VideoFrameConverter::new(frame_slot.buffer_pool());
    let mut current_start_position_nsecs = seconds_to_nsecs(source.start_position_seconds);
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
    let mut dovi_state = DoviMetadataState::default();
    let external_subtitle_cues = source
        .selected_tracks
        .subtitle_external_url
        .as_deref()
        .map(|url| {
            load_external_subtitle_cues(
                url,
                source.http_headers.as_slice(),
                source.selected_tracks.subtitle_codec.as_deref(),
            )
            .map(|cues| cues.into_iter().collect::<Vec<_>>())
            .map_err(|error| format!("加载外挂字幕失败：{error}"))
        })
        .transpose()?
        .unwrap_or_default();
    let mut subtitle_cues =
        subtitle_cue_queue_from_external(&external_subtitle_cues, current_start_position_nsecs);
    let mut active_subtitle = None;
    let mut subtitle_filter = match subtitle_stream {
        Some(stream) => PgsFrameMergeBitstreamFilter::new(stream)?,
        None => None,
    };
    let needs_subtitle_prefetch = subtitle_stream
        .is_some_and(|stream| stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE);
    let mut filtered_subtitle_packet = if subtitle_filter.is_some() {
        Some(AvPacket::new()?)
    } else {
        None
    };

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
            current_session_id,
            BackendEventKind::DurationChanged(duration),
        ));
    }
    let _ = event_tx.send(BackendEvent::new(
        current_session_id,
        BackendEventKind::PlaybackInfoChanged(playback_video_info(video_stream, &video_decoder)),
    ));
    let duration_seconds = input.duration_seconds();

    let mut packet = AvPacket::new()?;
    let mut buffered_reporter = BufferedReporter::new(audio_output.is_some());
    let mut queued_video_frames = VecDeque::new();
    let mut first_video_frame_pending = true;
    buffered_reporter.reset_to(
        source.start_position_seconds.max(0.0),
        current_session_id,
        &event_tx,
    );
    let _ = event_tx.send(BackendEvent::new(
        current_session_id,
        BackendEventKind::Buffering(true),
    ));
    let _ = event_tx.send(BackendEvent::new(
        current_session_id,
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

        if let Some(pending_seek) = drained_commands.pending_seek {
            current_session_id = pending_seek.session_id;
            control.set_session_id(current_session_id);
            control.set_paused(false);
            control.finish_seek(pending_seek.generation);
            let seek_result: std::result::Result<(), String> = (|| {
                let position_seconds = pending_seek.position_seconds.max(0.0);
                current_start_position_nsecs = seconds_to_nsecs(position_seconds);
                input.seek_stream(video_stream, position_seconds)?;
                video_decoder.flush_buffers();
                if let Some(decoder) = &audio_decoder {
                    decoder.flush_buffers();
                }
                if let Some(decoder) = &subtitle_decoder {
                    decoder.flush_buffers();
                }
                if let Some(filter) = subtitle_filter.as_mut() {
                    filter.flush();
                }
                if let Some(packet) = filtered_subtitle_packet.as_mut() {
                    packet.unref();
                }
                video_frame.unref();
                if let Some(frame) = audio_frame.as_mut() {
                    frame.unref();
                }
                packet.unref();
                video_clock = TimestampMapper::new(
                    video_stream.start_nsecs,
                    current_start_position_nsecs,
                    Some(video_frame_duration_nsecs),
                );
                playback_timeline_origin_nsecs = video_stream.start_nsecs;
                audio_clock = TimestampMapper::new(
                    audio_stream.and_then(|stream| stream.start_nsecs),
                    current_start_position_nsecs,
                    None,
                );
                scheduler.reset(current_start_position_nsecs);
                if let Some(output) = &audio_output {
                    output.reset_clock(current_start_position_nsecs);
                }
                queued_video_frames.clear();
                first_video_frame_pending = true;
                dovi_state.clear();
                subtitle_cues = subtitle_cue_queue_from_external(
                    &external_subtitle_cues,
                    current_start_position_nsecs,
                );
                active_subtitle = None;
                buffered_reporter = BufferedReporter::new(audio_output.is_some());
                buffered_reporter.reset_to(position_seconds, current_session_id, &event_tx);
                let _ = event_tx.send(BackendEvent::new(
                    current_session_id,
                    BackendEventKind::PositionChanged(position_seconds),
                ));
                let _ = event_tx.send(BackendEvent::new(
                    current_session_id,
                    BackendEventKind::Buffering(true),
                ));
                let _ = event_tx.send(BackendEvent::new(
                    current_session_id,
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
        if read == ffi::AVERROR_EOF {
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
                dovi_state.observe_packet(&packet, video_stream);
                video_decoder.decode_packet(packet.as_ptr(), &mut video_frame, |frame| {
                    if control.has_pending_seek() {
                        return Ok(());
                    }
                    let timestamp = video_clock
                        .map(frame_best_effort_timestamp(frame), video_decoder.time_base);
                    refresh_playback_timeline_origin(
                        &mut playback_timeline_origin_nsecs,
                        &video_clock,
                        subtitle_stream,
                        &mut subtitle_cues,
                    );
                    let frame_pts = FramePts {
                        nsecs: timestamp.timeline_nsecs,
                    };
                    if timestamp.timeline_nsecs < current_start_position_nsecs {
                        dovi_state.discard_frame(frame_pts);
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
                            dovi_state.discard_frame(frame_pts);
                            return Ok(());
                        }
                        if should_drop_backlogged_vulkan_frame(
                            frame,
                            first_video_frame_pending,
                            &frame_slot,
                        ) {
                            dovi_state.discard_frame(frame_pts);
                            return Ok(());
                        }

                        let dovi_metadata = dovi_state.metadata_for_frame(frame, frame_pts);
                        let mut decoded_frame =
                            video_converter.convert(&video_decoder, frame, dovi_metadata)?;
                        decoded_frame.pts = Some(frame_pts);
                        update_subtitle_overlay_from_audio_clock(
                            output,
                            &mut subtitle_cues,
                            &mut active_subtitle,
                            current_session_id,
                            &event_tx,
                        );

                        if first_video_frame_pending {
                            present_decoded_video_frame(
                                decoded_frame,
                                current_session_id,
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
                                current_session_id,
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
                            current_session_id,
                            &event_tx,
                        );
                        let played_until = present_due_audio_clocked_video_frames(
                            &mut queued_video_frames,
                            output,
                            current_session_id,
                            &frame_slot,
                            &frame_presented,
                            &mut position_reporter,
                            &event_tx,
                        );
                        update_subtitle_overlay(
                            played_until,
                            &mut subtitle_cues,
                            &mut active_subtitle,
                            current_session_id,
                            &event_tx,
                        );
                        if queued_video_duration(&queued_video_frames)
                            >= queued_video_limit_duration(
                                &queued_video_frames,
                                needs_subtitle_prefetch,
                            )
                        {
                            let target_duration = queued_video_target_duration(
                                &queued_video_frames,
                                needs_subtitle_prefetch,
                            );
                            wait_for_audio_clocked_video_queue(
                                &mut queued_video_frames,
                                output,
                                &control,
                                current_session_id,
                                &frame_slot,
                                &frame_presented,
                                &mut position_reporter,
                                &event_tx,
                                target_duration,
                                |played_until| {
                                    update_subtitle_overlay(
                                        played_until,
                                        &mut subtitle_cues,
                                        &mut active_subtitle,
                                        current_session_id,
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
                            dovi_state.discard_frame(frame_pts);
                            return Ok(());
                        }
                        let dovi_metadata = dovi_state.metadata_for_frame(frame, frame_pts);
                        let mut decoded_frame =
                            video_converter.convert(&video_decoder, frame, dovi_metadata)?;
                        decoded_frame.pts = Some(frame_pts);
                        update_subtitle_overlay(
                            timestamp.timeline_nsecs,
                            &mut subtitle_cues,
                            &mut active_subtitle,
                            current_session_id,
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
                            current_session_id,
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
                            current_session_id,
                            &event_tx,
                        );
                    }
                    Ok(())
                })
            }
            index
                if subtitle_decoder
                    .as_ref()
                    .is_some_and(|decoder| index == decoder.stream_index) =>
            {
                let decoder = subtitle_decoder
                    .as_ref()
                    .expect("subtitle decoder checked above");
                let stream = subtitle_stream.expect("subtitle stream exists with decoder");
                if let Some(filter) = subtitle_filter.as_mut() {
                    filter.send_packet(packet.as_mut_ptr())?;
                    let filtered_packet = filtered_subtitle_packet
                        .as_mut()
                        .expect("filtered subtitle packet exists with subtitle filter");
                    loop {
                        if !filter.receive_packet(filtered_packet)? {
                            break;
                        }
                        decode_subtitle_packet_into_queue(
                            decoder,
                            stream,
                            filtered_packet,
                            current_start_position_nsecs,
                            playback_timeline_origin_nsecs,
                            &mut subtitle_cues,
                            &control,
                        )?;
                        if let Some(output) = audio_output.as_ref() {
                            update_subtitle_overlay_from_audio_clock(
                                output,
                                &mut subtitle_cues,
                                &mut active_subtitle,
                                current_session_id,
                                &event_tx,
                            );
                        }
                        filtered_packet.unref();
                    }
                    Ok(())
                } else {
                    decode_subtitle_packet_into_queue(
                        decoder,
                        stream,
                        &packet,
                        current_start_position_nsecs,
                        playback_timeline_origin_nsecs,
                        &mut subtitle_cues,
                        &control,
                    )?;
                    if let Some(output) = audio_output.as_ref() {
                        update_subtitle_overlay_from_audio_clock(
                            output,
                            &mut subtitle_cues,
                            &mut active_subtitle,
                            current_session_id,
                            &event_tx,
                        );
                    }
                    Ok(())
                }
            }
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
                                current_session_id,
                                &frame_slot,
                                &frame_presented,
                                &mut position_reporter,
                                &event_tx,
                            );
                            update_subtitle_overlay(
                                played_until,
                                &mut subtitle_cues,
                                &mut active_subtitle,
                                current_session_id,
                                &event_tx,
                            );
                            Ok(())
                        })?;
                        buffered_reporter.report_audio_timeline_nsecs(
                            buffered_until_nsecs,
                            current_session_id,
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
        refresh_playback_timeline_origin(
            &mut playback_timeline_origin_nsecs,
            &video_clock,
            subtitle_stream,
            &mut subtitle_cues,
        );
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
                dovi_state.discard_frame(frame_pts);
                return Ok(());
            }
            if should_drop_backlogged_vulkan_frame(frame, first_video_frame_pending, &frame_slot) {
                dovi_state.discard_frame(frame_pts);
                return Ok(());
            }

            let dovi_metadata = dovi_state.metadata_for_frame(frame, frame_pts);
            let mut decoded_frame =
                video_converter.convert(&video_decoder, frame, dovi_metadata)?;
            decoded_frame.pts = Some(frame_pts);
            update_subtitle_overlay_from_audio_clock(
                output,
                &mut subtitle_cues,
                &mut active_subtitle,
                current_session_id,
                &event_tx,
            );

            if first_video_frame_pending {
                present_decoded_video_frame(
                    decoded_frame,
                    current_session_id,
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
                    current_session_id,
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
                current_session_id,
                &event_tx,
            );
            let played_until = present_due_audio_clocked_video_frames(
                &mut queued_video_frames,
                output,
                current_session_id,
                &frame_slot,
                &frame_presented,
                &mut position_reporter,
                &event_tx,
            );
            update_subtitle_overlay(
                played_until,
                &mut subtitle_cues,
                &mut active_subtitle,
                current_session_id,
                &event_tx,
            );
        } else {
            if should_drop_backlogged_vulkan_frame(frame, first_video_frame_pending, &frame_slot) {
                dovi_state.discard_frame(frame_pts);
                return Ok(());
            }
            let dovi_metadata = dovi_state.metadata_for_frame(frame, frame_pts);
            let mut decoded_frame =
                video_converter.convert(&video_decoder, frame, dovi_metadata)?;
            decoded_frame.pts = Some(frame_pts);
            update_subtitle_overlay(
                timestamp.timeline_nsecs,
                &mut subtitle_cues,
                &mut active_subtitle,
                current_session_id,
                &event_tx,
            );

            first_video_frame_pending = false;
            if scheduler
                .wait_until(timestamp.timeline_nsecs, &control)
                .interrupted()
            {
                return Ok(());
            }
            present_decoded_video_frame(
                decoded_frame,
                current_session_id,
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
                current_session_id,
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
                        current_session_id,
                        &frame_slot,
                        &frame_presented,
                        &mut position_reporter,
                        &event_tx,
                    );
                    update_subtitle_overlay(
                        played_until,
                        &mut subtitle_cues,
                        &mut active_subtitle,
                        current_session_id,
                        &event_tx,
                    );
                    Ok(())
                })?;
                buffered_reporter.report_audio_timeline_nsecs(
                    buffered_until_nsecs,
                    current_session_id,
                    &event_tx,
                );
            }
            Ok(())
        })?;
    }

    buffered_reporter.report_value(duration_seconds, current_session_id, &event_tx);
    if let Some(output) = &audio_output {
        drain_audio_clocked_video_queue(
            &mut queued_video_frames,
            output,
            &control,
            current_session_id,
            &frame_slot,
            &frame_presented,
            &mut position_reporter,
            &event_tx,
            |played_until| {
                update_subtitle_overlay(
                    played_until,
                    &mut subtitle_cues,
                    &mut active_subtitle,
                    current_session_id,
                    &event_tx,
                );
            },
        )?;
        output.drain(&control)?;
    }
    Ok(())
}

fn should_drop_backlogged_vulkan_frame(
    frame: *const ffi::AVFrame,
    first_video_frame_pending: bool,
    frame_slot: &FrameSlot,
) -> bool {
    if first_video_frame_pending || !is_vulkan_frame(frame) {
        return false;
    }
    let key_frame = unsafe { (*frame).flags & ffi::AV_FRAME_FLAG_KEY != 0 };
    !key_frame && frame_slot.render_backpressure().should_drop_non_key_frame()
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

fn update_subtitle_overlay_from_audio_clock(
    audio_output: &AudioOutput,
    cues: &mut VecDeque<BackendSubtitleCue>,
    active: &mut Option<BackendSubtitleCue>,
    session_id: PlaybackSessionId,
    event_tx: &Sender<BackendEvent>,
) {
    update_subtitle_overlay(
        audio_output.played_timeline_nsecs(),
        cues,
        active,
        session_id,
        event_tx,
    );
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
