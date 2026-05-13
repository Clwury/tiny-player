use super::*;

struct OpenedPlaybackInput {
    input: FormatContext,
    video_stream: StreamInfo,
    video_decoder: Decoder,
    audio_stream: Option<StreamInfo>,
    audio_decoder: Option<Decoder>,
}

fn open_playback_input_with_fallback(
    source: &FfmpegPlaybackInput,
    control: Arc<FfmpegControl>,
    event_tx: &Sender<BackendEvent>,
) -> std::result::Result<OpenedPlaybackInput, String> {
    match open_playback_input(
        source,
        Arc::clone(&control),
        event_tx,
        InputProbeProfile::Fast,
        false,
    ) {
        Ok(opened) if opened.audio_stream.is_some() => Ok(opened),
        Ok(opened) => {
            tracing::debug!("FFmpeg fast probe did not find audio stream; retrying full probe");
            match open_playback_input(source, control, event_tx, InputProbeProfile::Full, true) {
                Ok(opened) => Ok(opened),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "FFmpeg full probe fallback failed; continuing with fast probe result"
                    );
                    Ok(opened)
                }
            }
        }
        Err(fast_error) => {
            tracing::debug!(%fast_error, "FFmpeg fast probe failed; retrying full probe");
            open_playback_input(source, control, event_tx, InputProbeProfile::Full, true).map_err(
                |full_error| {
                    format!("FFmpeg 快速探测失败：{fast_error}；完整探测也失败：{full_error}")
                },
            )
        }
    }
}

fn open_playback_input(
    source: &FfmpegPlaybackInput,
    control: Arc<FfmpegControl>,
    event_tx: &Sender<BackendEvent>,
    probe_profile: InputProbeProfile,
    allow_audio_decoder_failure: bool,
) -> std::result::Result<OpenedPlaybackInput, String> {
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
    let video_decoder = Decoder::open_video(video_stream, HardwareDecodeMode::from_env())
        .map_err(|error| format!("FFmpeg 打开视频解码器失败：{error}"))?;
    let (audio_stream, audio_decoder) = open_audio_decoder(&input, allow_audio_decoder_failure)?;

    Ok(OpenedPlaybackInput {
        input,
        video_stream,
        video_decoder,
        audio_stream,
        audio_decoder,
    })
}

fn open_audio_decoder(
    input: &FormatContext,
    allow_audio_decoder_failure: bool,
) -> std::result::Result<(Option<StreamInfo>, Option<Decoder>), String> {
    let audio_stream = match input.best_stream(ffi::AVMediaType::AVMEDIA_TYPE_AUDIO) {
        Ok(stream) => stream,
        Err(error) if allow_audio_decoder_failure => {
            tracing::warn!(%error, "FFmpeg audio stream selection failed");
            None
        }
        Err(error) => return Err(format!("FFmpeg 选择音频流失败：{error}")),
    };
    let Some(stream) = audio_stream else {
        return Ok((None, None));
    };

    match Decoder::open_audio(stream) {
        Ok(decoder) => Ok((Some(stream), Some(decoder))),
        Err(error) if allow_audio_decoder_failure => {
            tracing::warn!(%error, "FFmpeg audio decoder initialization failed");
            Ok((Some(stream), None))
        }
        Err(error) => Err(format!("FFmpeg 打开音频解码器失败：{error}")),
    }
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
    } = open_playback_input_with_fallback(&source, Arc::clone(&control), &event_tx)?;
    if source.start_position_seconds > 0.0 {
        input.seek_stream(video_stream, source.start_position_seconds)?;
    }
    let mut video_frame = AvFrame::new()?;
    let mut video_converter = VideoFrameConverter::new(frame_slot.buffer_pool());
    let mut current_start_position_nsecs = seconds_to_nsecs(source.start_position_seconds);
    let video_frame_duration_nsecs = video_stream
        .frame_duration_nsecs
        .unwrap_or(DEFAULT_VIDEO_FRAME_DURATION_NSECS);
    let mut video_clock = TimestampMapper::new(
        video_stream.start_nsecs,
        current_start_position_nsecs,
        Some(video_frame_duration_nsecs),
    );
    let mut scheduler = PlaybackScheduler::new(current_start_position_nsecs);
    let mut position_reporter = PositionReporter::default();
    let mut dovi_queue = DoviMetadataQueue::default();

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
                dovi_queue.clear();
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
                dovi_queue.observe_packet(&packet, video_stream);
                video_decoder.decode_packet(packet.as_ptr(), &mut video_frame, |frame| {
                    if control.has_pending_seek() {
                        return Ok(());
                    }
                    let timestamp = video_clock
                        .map(frame_best_effort_timestamp(frame), video_decoder.time_base);
                    let frame_pts = FramePts {
                        nsecs: timestamp.timeline_nsecs,
                    };
                    if timestamp.timeline_nsecs < current_start_position_nsecs {
                        let _ = dovi_queue.take_for_frame(frame_pts);
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
                            let _ = dovi_queue.take_for_frame(frame_pts);
                            return Ok(());
                        }
                        if should_drop_backlogged_vulkan_frame(
                            frame,
                            first_video_frame_pending,
                            &frame_slot,
                        ) {
                            let _ = dovi_queue.take_for_frame(frame_pts);
                            return Ok(());
                        }

                        let dovi_metadata = dovi_metadata_from_frame(frame)
                            .or_else(|| dovi_queue.take_for_frame(frame_pts));
                        let mut decoded_frame =
                            video_converter.convert(&video_decoder, frame, dovi_metadata)?;
                        decoded_frame.pts = Some(frame_pts);

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
                        present_due_audio_clocked_video_frames(
                            &mut queued_video_frames,
                            output,
                            current_session_id,
                            &frame_slot,
                            &frame_presented,
                            &mut position_reporter,
                            &event_tx,
                        );
                        if queued_video_duration(&queued_video_frames)
                            >= queued_video_limit_duration(&queued_video_frames)
                        {
                            let target_duration =
                                queued_video_target_duration(&queued_video_frames);
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
                            )?;
                        }
                    } else {
                        if should_drop_backlogged_vulkan_frame(
                            frame,
                            first_video_frame_pending,
                            &frame_slot,
                        ) {
                            let _ = dovi_queue.take_for_frame(frame_pts);
                            return Ok(());
                        }
                        let dovi_metadata = dovi_metadata_from_frame(frame)
                            .or_else(|| dovi_queue.take_for_frame(frame_pts));
                        let mut decoded_frame =
                            video_converter.convert(&video_decoder, frame, dovi_metadata)?;
                        decoded_frame.pts = Some(frame_pts);

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
                            present_due_audio_clocked_video_frames(
                                &mut queued_video_frames,
                                output,
                                current_session_id,
                                &frame_slot,
                                &frame_presented,
                                &mut position_reporter,
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
                let _ = dovi_queue.take_for_frame(frame_pts);
                return Ok(());
            }
            if should_drop_backlogged_vulkan_frame(frame, first_video_frame_pending, &frame_slot) {
                let _ = dovi_queue.take_for_frame(frame_pts);
                return Ok(());
            }

            let dovi_metadata =
                dovi_metadata_from_frame(frame).or_else(|| dovi_queue.take_for_frame(frame_pts));
            let mut decoded_frame =
                video_converter.convert(&video_decoder, frame, dovi_metadata)?;
            decoded_frame.pts = Some(frame_pts);

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
            present_due_audio_clocked_video_frames(
                &mut queued_video_frames,
                output,
                current_session_id,
                &frame_slot,
                &frame_presented,
                &mut position_reporter,
                &event_tx,
            );
        } else {
            if should_drop_backlogged_vulkan_frame(frame, first_video_frame_pending, &frame_slot) {
                let _ = dovi_queue.take_for_frame(frame_pts);
                return Ok(());
            }
            let dovi_metadata =
                dovi_metadata_from_frame(frame).or_else(|| dovi_queue.take_for_frame(frame_pts));
            let mut decoded_frame =
                video_converter.convert(&video_decoder, frame, dovi_metadata)?;
            decoded_frame.pts = Some(frame_pts);

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
                    present_due_audio_clocked_video_frames(
                        &mut queued_video_frames,
                        output,
                        current_session_id,
                        &frame_slot,
                        &frame_presented,
                        &mut position_reporter,
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
