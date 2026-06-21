use std::sync::{
    Arc,
    atomic::AtomicBool,
    mpsc::{Receiver, Sender},
};

#[cfg(test)]
use std::os::raw::c_int;

#[cfg(test)]
use ffmpeg_sys_next as ffi;

use crate::player::{
    backend::{BackendEvent, BackendEventKind, PlaybackVideoInfo},
    render_host::VideoOutputQueue,
};

use super::{
    AudioDecodePipeline, AudioOutput, BufferedReporter, DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    DemuxPacketCache, DemuxPacketCacheInput, DoviPipeline,
    END_OF_PLAYBACK_READ_ERROR_TOLERANCE_SECONDS, FfmpegCommand, FfmpegControl,
    FfmpegPlaybackInput, OpenedPlaybackInput, PlaybackCommandContext, PlaybackCommandServiceStatus,
    PlaybackCoordinatorGateContext, PlaybackEofDrainContext, PlaybackEofDrainStatus,
    PlaybackGeneration, PlaybackOutputScheduler, PlaybackPipelineServices, PlaybackPipelineState,
    PlaybackScheduler, PlaybackSession, PlaybackTickContext, PlaybackTickStatus, PositionReporter,
    StreamInfo, SubtitlePipeline, TimestampMapper, VideoDecodePipeline, VideoDecodeRecovery,
    VideoDecodeWorkerInfo, VideoFramePrepareWorker, open_playback_input_with_fallback,
    preroll_seek_position_seconds, service_playback_commands, service_playback_eof_drain,
    service_playback_tick, should_cache_http_url, video_seek_preroll_nsecs,
};

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

pub(in crate::player::backend::ffmpeg) fn run_ffmpeg_playback(
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
                http_cache: http_cache.as_ref(),
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
pub(in crate::player::backend::ffmpeg) fn playback_read_finished(
    read_result: c_int,
    duration_seconds: Option<f64>,
    buffered_until_seconds: Option<f64>,
) -> bool {
    read_result == ffi::AVERROR_EOF
        || (read_result == ffi::AVERROR(ffi::EIO)
            && playback_buffered_near_duration(duration_seconds, buffered_until_seconds))
}

pub(super) fn playback_buffered_near_duration(
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
