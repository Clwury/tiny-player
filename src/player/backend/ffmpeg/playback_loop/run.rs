use std::sync::{
    Arc,
    atomic::AtomicBool,
    mpsc::{Receiver, Sender},
};
use std::time::Instant;

#[cfg(test)]
use std::os::raw::c_int;

use ffmpeg_sys_next as ffi;

use crate::player::{
    backend::{BackendEvent, BackendEventKind},
    render_host::{PlaybackSessionId, VideoOutputQueue, VulkanPrewarmStatus},
};

use super::decode_pipeline_service::{DecodePipelineService, DecodePipelineServiceContext};
use super::decoder_input_service::{DecoderInputServiceContext, DecoderInputServiceOutcome};
use super::demux_cache::DemuxSeekResult;
use super::demux_packet_pump::cached_input_output_lead_throttled;
use super::output_gate::DecodeRecoverySource;
use super::output_gate_service::OutputGateServiceContext;
use super::playback_pipeline_state::{
    AudioRealignRequestAction, AudioRecoveryWatchdogAction, CachedSeekRecoveryFallback,
    CachedSeekRecoveryFallbackAction, CachedSeekRecoveryFallbackReason,
};
use super::playback_reset_service::{
    PlaybackSeekBufferingPolicy, PlaybackSeekResetContext, service_playback_seek_reset,
};
use super::playback_wait_service::PlaybackPipelineWaitService;
use super::video_decode_pipeline::{
    HevcDecodeChainFallback, HevcDecodeChainFallbackLoopAction, HevcDecodeChainFallbackReason,
    HevcDecodeRecoveryAction, hevc_drain_video_result_progressed,
};
use super::{
    AudioDecodePipeline, AudioOutput, AudioRealignCoverage, BufferedReporter,
    DEFAULT_VIDEO_FRAME_DURATION_NSECS, DemuxPacketCache, DemuxPacketCacheInput,
    DemuxReaderWatermark, DoviPipeline, END_OF_PLAYBACK_READ_ERROR_TOLERANCE_SECONDS,
    FfmpegCommand, FfmpegControl, FfmpegPlaybackInput, OpenedPlaybackInput, PlaybackCommandContext,
    PlaybackCommandServiceStatus, PlaybackCoordinatorGateContext, PlaybackCoordinatorGateStatus,
    PlaybackEofDrainContext, PlaybackEofDrainStatus, PlaybackGeneration, PlaybackOutputScheduler,
    PlaybackOutputSnapshot, PlaybackPipelineServices, PlaybackPipelineState,
    PlaybackRecoveryRequest, PlaybackRecoverySource, PlaybackScheduler, PlaybackSession,
    PlaybackTickContext, PlaybackTickStatus, PositionReporter, RebufferAudioRealignRequest,
    SubtitlePipeline, TimestampMapper, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
    VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE, VideoDecodePipeline, VideoDecodeRecovery,
    VideoFramePrepareWorker, audio_codec_requires_recovery_point, duration_nsecs,
    expire_initial_av_start_hard_deadline, nsecs_to_seconds, open_playback_input_with_fallback,
    playback_audio_info_from_stream, playback_video_info_from_worker,
    preroll_seek_position_seconds, seconds_to_nsecs, service_hevc_startup_stall_watchdog_if_due,
    service_playback_commands, service_playback_eof_drain, service_playback_tick,
    should_cache_http_url, video_seek_preroll_nsecs,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryFallbackArbitration<Cached, Hevc> {
    CachedSeek(Cached),
    HevcDecodeChain {
        request: Option<PlaybackRecoveryRequest>,
        fallback: Hevc,
    },
    MissingRequested(PlaybackRecoveryRequest),
    None,
}

trait RecoveryFallbackSource {
    type CachedFallback;
    type HevcFallback;

    fn take_cached_seek_fallback(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> Option<Self::CachedFallback>;
    fn take_hevc_decode_chain_fallback(&mut self) -> Option<Self::HevcFallback>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CachedInputAdmission {
    input_admissible: bool,
    output_transaction_blocked: bool,
}

fn cached_input_admission(
    requested_input_drainable: bool,
    output_lead_throttled: bool,
    _output_snapshot: PlaybackOutputSnapshot,
) -> CachedInputAdmission {
    CachedInputAdmission {
        input_admissible: requested_input_drainable && !output_lead_throttled,
        // Primed transactions are bounded by the decoded A/V queue limits.
        // AudioStartDue is a retry hint, not exclusive ownership of input.
        output_transaction_blocked: false,
    }
}

impl RecoveryFallbackSource for PlaybackPipelineState {
    type CachedFallback = CachedSeekRecoveryFallback;
    type HevcFallback = HevcDecodeChainFallback;

    fn take_cached_seek_fallback(
        &mut self,
        session_id: PlaybackSessionId,
    ) -> Option<Self::CachedFallback> {
        self.take_cached_seek_recovery_fallback(session_id)
    }

    fn take_hevc_decode_chain_fallback(&mut self) -> Option<Self::HevcFallback> {
        self.video_decode_pipeline.take_hevc_decode_chain_fallback()
    }
}

fn take_next_recovery_fallback<Source>(
    source: &mut Source,
    session_id: PlaybackSessionId,
    requested_recovery: Option<PlaybackRecoveryRequest>,
) -> RecoveryFallbackArbitration<Source::CachedFallback, Source::HevcFallback>
where
    Source: RecoveryFallbackSource,
{
    if let Some(request) = requested_recovery {
        return source
            .take_hevc_decode_chain_fallback()
            .map(|fallback| RecoveryFallbackArbitration::HevcDecodeChain {
                request: Some(request),
                fallback,
            })
            .unwrap_or(RecoveryFallbackArbitration::MissingRequested(request));
    }
    if let Some(fallback) = source.take_cached_seek_fallback(session_id) {
        return RecoveryFallbackArbitration::CachedSeek(fallback);
    }
    source
        .take_hevc_decode_chain_fallback()
        .map(|fallback| RecoveryFallbackArbitration::HevcDecodeChain {
            request: None,
            fallback,
        })
        .unwrap_or(RecoveryFallbackArbitration::None)
}

#[derive(Default)]
struct MissingRecoveryRequestTracker {
    request: Option<PlaybackRecoveryRequest>,
    misses: u64,
}

impl MissingRecoveryRequestTracker {
    fn record(&mut self, request: PlaybackRecoveryRequest) -> bool {
        if self.request != Some(request) {
            self.request = Some(request);
            self.misses = 1;
            return true;
        }
        self.misses = self.misses.saturating_add(1);
        false
    }

    fn take_summary(&mut self) -> Option<(PlaybackRecoveryRequest, u64)> {
        let request = self.request.take()?;
        let misses = std::mem::take(&mut self.misses);
        Some((request, misses))
    }
}

fn wait_after_missing_recovery_request(
    pipeline: &mut PlaybackPipelineState,
    playback_wait: &PlaybackPipelineWaitService,
    tracker: &mut MissingRecoveryRequestTracker,
    request: PlaybackRecoveryRequest,
    session_id: crate::player::render_host::PlaybackSessionId,
    checkpoint: &'static str,
) {
    if tracker.record(request) {
        tracing::error!(
            ?session_id,
            checkpoint,
            transaction_id = request.transaction_id,
            recovery_source = request.source.as_str(),
            target_nsecs = request.target_nsecs,
            arbitration_outcome = "missing_request_wait",
            missing_request_count = 1,
            "playback recovery action had no matching pending fallback; entering bounded wait"
        );
    }
    playback_wait.wait_after_missing_recovery_request(&mut pipeline.scheduler);
}

fn log_recovery_request_miss_summary(
    tracker: &mut MissingRecoveryRequestTracker,
    session_id: crate::player::render_host::PlaybackSessionId,
) {
    let Some((request, misses)) = tracker.take_summary() else {
        return;
    };
    tracing::warn!(
        ?session_id,
        transaction_id = request.transaction_id,
        recovery_source = request.source.as_str(),
        target_nsecs = request.target_nsecs,
        arbitration_outcome = "missing_request_cleared",
        missing_request_count = misses,
        "cleared aggregated missing playback recovery request state"
    );
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
    let initial_playback_file_info = input.playback_file_info();
    let mut video_decode_pipeline = VideoDecodePipeline::spawn(video_decoder)?;
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
        if video_stream.codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC {
            let transaction_id = 1;
            let armed = video_decode_pipeline.begin_hevc_low_level_seek_observation(
                transaction_id,
                seconds_to_nsecs(source.start_position_seconds),
                seconds_to_nsecs(seek_position_seconds),
                "initial_resume",
            );
            tracing::debug!(
                session_id = ?source.session_id,
                transaction_id,
                recovery_scope = "exact_low_level_seek",
                target_nsecs = seconds_to_nsecs(source.start_position_seconds),
                seek_position_nsecs = seconds_to_nsecs(seek_position_seconds),
                armed,
                "armed HEVC exact low-level recovery for initial resume"
            );
        }
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
    let initial_playback_audio_info =
        playback_audio_info_from_stream(audio_stream, audio_output.as_ref());

    if let Some(duration) = duration_seconds {
        let _ = event_tx.send(BackendEvent::new(
            session.id(),
            BackendEventKind::DurationChanged(duration),
        ));
    }
    let _ = event_tx.send(BackendEvent::new(
        session.id(),
        BackendEventKind::PlaybackFileInfoChanged(initial_playback_file_info),
    ));
    let _ = event_tx.send(BackendEvent::new(
        session.id(),
        BackendEventKind::PlaybackInfoChanged(initial_playback_video_info),
    ));
    let _ = event_tx.send(BackendEvent::new(
        session.id(),
        BackendEventKind::PlaybackAudioInfoChanged(initial_playback_audio_info),
    ));
    let emit_playback_buffered_events = false;
    let buffered_reporter =
        BufferedReporter::new_with_events(audio_output.is_some(), emit_playback_buffered_events);
    let mut output_scheduler = PlaybackOutputScheduler::new();
    output_scheduler.start_video_deadline_service(
        audio_output.as_ref().map(AudioOutput::clock_handle),
        session.id(),
        video_output_queue.clone(),
        Arc::clone(&frame_presented),
        event_tx.clone(),
    )?;
    let mut video_decode_recovery = VideoDecodeRecovery::default();
    video_decode_recovery
        .reset_for_timeline_start(video_stream.codec_id, current_start_position_nsecs);
    let mut pipeline_services = PlaybackPipelineServices::new(Arc::clone(&control));
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
        initial_hevc_cached_exact_seek: false,
        cached_seek_recovery_watchdog: None,
        cached_seek_recovery_attempt: None,
        audio_realign_transaction: None,
        audio_realign_retained_pending: None,
        audio_realign_retained_decoded_frames: Vec::new(),
        next_recovery_transaction_id: 2,
        active_recovery_transaction_id: 1,
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
    let mut missing_recovery_request_tracker = MissingRecoveryRequestTracker::default();

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

            // This is the first post-command coordinator action: no AO status
            // probe or recovery service is allowed to hide a terminal expiry.
            let output_demand_before_snapshots = pipeline
                .output_scheduler
                .output_service_demand(Instant::now());
            if output_demand_before_snapshots.hard_deadline_due()
                && expire_initial_av_start_hard_deadline(
                    &mut pipeline.output_scheduler,
                    pipeline.audio_output.as_ref(),
                    Instant::now(),
                    &control,
                    session.id(),
                )
            {
                continue;
            }

            if service_hevc_same_hardware_recovery_if_needed(
                &mut session,
                &control,
                &demux_cache,
                &mut pipeline,
                &video_output_queue,
                &event_tx,
                emit_playback_buffered_events,
                &pipeline_services.wait,
                &mut pipeline_services.decode_pipeline,
                &frame_presented,
            )? {
                continue;
            }

            if service_cached_seek_recovery_fallback_if_needed(
                &mut session,
                &control,
                &demux_cache,
                &mut pipeline,
                &video_output_queue,
                &event_tx,
                emit_playback_buffered_events,
                None,
            )? {
                log_recovery_request_miss_summary(
                    &mut missing_recovery_request_tracker,
                    session.id(),
                );
                continue;
            }

            if service_audio_realign_recovery_watchdog_if_needed(
                &mut session,
                &control,
                &demux_cache,
                &mut pipeline,
                &video_output_queue,
                &event_tx,
                emit_playback_buffered_events,
            )? {
                continue;
            }

            if service_hevc_startup_stall_watchdog_due_if_needed(
                &mut session,
                &control,
                &demux_cache,
                &mut pipeline,
                &video_output_queue,
                &event_tx,
                emit_playback_buffered_events,
                &pipeline_services.wait,
                &mut pipeline_services.decode_pipeline,
                &frame_presented,
                &mut missing_recovery_request_tracker,
                "coordinator_gate_enter",
            )? {
                continue;
            }

            let playback_loop_deadline = pipeline.playback_loop_deadline();
            let cache_pause_work = pipeline.cache_pause_work_snapshot();
            let (demux_packet_snapshot, demux_reader_watermark, _) = demux_cache.monitor_snapshot();
            let cached_input_drainable = demux_packet_snapshot
                .consumer_drainable_for_streams(&cache_pause_work.selected_streams);
            let requested_input_drainable = demux_packet_snapshot
                .consumer_drainable_for_streams(&cache_pause_work.requested_streams);
            let output_snapshot = pipeline.output_scheduler.snapshot();
            let output_backpressure_prefetch_paused =
                pipeline.output_backpressure_prefetch_should_pause();
            demux_cache
                .set_output_backpressure_prefetch_paused(output_backpressure_prefetch_paused);
            if let Some(http_cache) = http_cache.as_ref() {
                http_cache
                    .set_output_backpressure_prefetch_paused(output_backpressure_prefetch_paused);
                http_cache.update_demux_high_water_prefetch_paused(
                    demux_packet_snapshot.total_bytes,
                    demux_packet_snapshot.memory_limit_bytes,
                    demux_packet_snapshot.prefetch_queue_full(),
                    demux_reader_watermark.underrun,
                );
            }
            let output_reference_nsecs = pipeline
                .audio_output
                .as_ref()
                .and_then(|output| output.try_snapshot().ok().flatten())
                .map(|snapshot| snapshot.played_timeline_nsecs)
                .unwrap_or(pipeline.current_start_position_nsecs);
            let output_lead_throttled = cached_input_output_lead_throttled(
                &demux_packet_snapshot,
                &cache_pause_work.requested_streams,
                output_snapshot,
                output_reference_nsecs,
            );
            let cached_input_admission = cached_input_admission(
                requested_input_drainable,
                output_lead_throttled,
                output_snapshot,
            );
            let cached_video = demux_packet_snapshot
                .streams
                .iter()
                .find(|stream| stream.stream_index == cache_pause_work.video_stream_index);
            let actual_anchor_nsecs = pipeline.exact_seek_actual_anchor_nsecs();
            let exact_seek_target_nsecs = demux_packet_snapshot.exact_seek_target_nsecs;
            let preroll_debt_nsecs = actual_anchor_nsecs
                .map(|anchor_nsecs| exact_seek_target_nsecs.saturating_sub(anchor_nsecs));
            let output_service_demand = pipeline
                .output_scheduler
                .output_service_demand(Instant::now());
            let coordinator_gate_status =
                pipeline_services
                    .coordinator_gate
                    .service(PlaybackCoordinatorGateContext {
                        control: &control,
                        output_scheduler: &pipeline.output_scheduler,
                        scheduler: &mut pipeline.scheduler,
                        playback_wait: &pipeline_services.wait,
                        playback_loop_deadline,
                        actual_decode_work: cache_pause_work.actual_decode_work,
                        output_service_demand,
                        first_frame_input_demand: cache_pause_work.first_frame_input_demand,
                        cached_input_drainable,
                        cached_input_admissible: cached_input_admission.input_admissible,
                        output_lead_throttled,
                        output_transaction_blocked: cached_input_admission
                            .output_transaction_blocked,
                        cache_generation: demux_packet_snapshot.cache_generation,
                        selected_streams: &cache_pause_work.selected_streams,
                        requested_streams: &cache_pause_work.requested_streams,
                        cached_streams: &demux_packet_snapshot.streams,
                        exact_seek_target_nsecs,
                        actual_anchor_nsecs,
                        preroll_debt_nsecs,
                        cached_video_end_nsecs: cached_video
                            .and_then(|stream| stream.cached_end_nsecs),
                        cached_video_drainable_packets: cached_video
                            .filter(|stream| stream.consumer_drainable)
                            .map(|stream| stream.readable_packets_for_stream)
                            .unwrap_or_default(),
                    });
            if coordinator_gate_status != PlaybackCoordinatorGateStatus::Ready {
                let drain_made_progress = match coordinator_gate_status {
                    PlaybackCoordinatorGateStatus::ServiceOutput => {
                        let _status = pipeline_services.output_gate.service_or_wait(
                            OutputGateServiceContext {
                                session_id: session.id(),
                                demux_cache: &demux_cache,
                                http_cache: http_cache.as_ref(),
                                pipeline: &mut pipeline,
                                control: &control,
                                event_tx: &event_tx,
                                vo_queue: &video_output_queue,
                                frame_presented: &frame_presented,
                                playback_wait: &pipeline_services.wait,
                                playback_telemetry: &mut pipeline_services.telemetry,
                                output_service_demand,
                            },
                        )?;
                        // Consuming a service demand changes its generation/deadline state.
                        // Re-evaluate immediately so decoded work or cached input can run;
                        // sleeping for the same interval as the probe recreated the demand
                        // and starved both paths indefinitely.
                        Some(true)
                    }
                    PlaybackCoordinatorGateStatus::DrainDecodeOnly => {
                        let drain_status = pipeline_services.decode_pipeline.service_once(
                            DecodePipelineServiceContext {
                                pipeline: &mut pipeline,
                                control: &control,
                                session_id: session.id(),
                                event_tx: &event_tx,
                                vo_queue: &video_output_queue,
                                frame_presented: &frame_presented,
                                demux_reader_watermark: || demux_cache.cached_reader_watermark(),
                            },
                        )?;
                        let retry_status = pipeline.retry_pending_decoder_inputs(session.id())?;
                        let made_progress =
                            drain_status.made_progress() || retry_status.made_progress();
                        if made_progress {
                            pipeline
                                .video_decode_pipeline
                                .observe_hevc_decode_pipeline_progress(Instant::now());
                        }
                        Some(made_progress)
                    }
                    PlaybackCoordinatorGateStatus::DrainCachedInput => {
                        let video_admission_pressure = pipeline.video_packet_admission_pressure(
                            Some(pipeline.current_start_position_nsecs),
                            pipeline.audio_output.is_some(),
                            video_output_queue.snapshot(),
                        );
                        let outcome = pipeline_services.decoder_input.service_cached_input(
                            DecoderInputServiceContext {
                                session_id: session.id(),
                                demux_cache: &demux_cache,
                                pipeline: &mut pipeline,
                                video_admission_pressure,
                                control: &control,
                                should_wait_for_demux: false,
                                video_output_waiting_for_demux: false,
                            },
                        )?;
                        if outcome == DecoderInputServiceOutcome::OutputLeadThrottled {
                            let playback_loop_deadline = pipeline.playback_loop_deadline();
                            pipeline_services
                                .wait
                                .wait_for_cache_generation_change_and_delay_scheduler_until(
                                    &mut pipeline.scheduler,
                                    &demux_cache,
                                    demux_packet_snapshot.cache_generation,
                                    playback_loop_deadline,
                                );
                            Some(true)
                        } else {
                            let made_progress =
                                matches!(outcome, DecoderInputServiceOutcome::Ready);
                            if made_progress {
                                pipeline
                                    .video_decode_pipeline
                                    .observe_hevc_decode_pipeline_progress(Instant::now());
                            }
                            Some(made_progress)
                        }
                    }
                    PlaybackCoordinatorGateStatus::Ready
                    | PlaybackCoordinatorGateStatus::WaitForStateChange
                    | PlaybackCoordinatorGateStatus::WaitForCache
                    | PlaybackCoordinatorGateStatus::Wait => None,
                };
                if service_hevc_startup_stall_watchdog_due_if_needed(
                    &mut session,
                    &control,
                    &demux_cache,
                    &mut pipeline,
                    &video_output_queue,
                    &event_tx,
                    emit_playback_buffered_events,
                    &pipeline_services.wait,
                    &mut pipeline_services.decode_pipeline,
                    &frame_presented,
                    &mut missing_recovery_request_tracker,
                    "coordinator_gate_continue",
                )? {
                    continue;
                }
                if service_cached_seek_recovery_fallback_if_needed(
                    &mut session,
                    &control,
                    &demux_cache,
                    &mut pipeline,
                    &video_output_queue,
                    &event_tx,
                    emit_playback_buffered_events,
                    None,
                )? {
                    log_recovery_request_miss_summary(
                        &mut missing_recovery_request_tracker,
                        session.id(),
                    );
                    continue;
                }
                if coordinator_gate_status == PlaybackCoordinatorGateStatus::WaitForCache
                    || (coordinator_gate_status == PlaybackCoordinatorGateStatus::DrainCachedInput
                        && drain_made_progress == Some(false))
                {
                    let playback_loop_deadline = pipeline.playback_loop_deadline();
                    pipeline_services
                        .wait
                        .wait_for_cached_input_and_delay_scheduler_until(
                            &mut pipeline.scheduler,
                            &demux_cache,
                            &cache_pause_work.selected_streams,
                            playback_loop_deadline,
                        );
                } else if coordinator_gate_status
                    == PlaybackCoordinatorGateStatus::WaitForStateChange
                {
                    let playback_loop_deadline = pipeline.playback_loop_deadline();
                    pipeline_services
                        .wait
                        .wait_for_cache_generation_change_and_delay_scheduler_until(
                            &mut pipeline.scheduler,
                            &demux_cache,
                            demux_packet_snapshot.cache_generation,
                            playback_loop_deadline,
                        );
                } else if drain_made_progress == Some(false) {
                    let playback_loop_deadline = pipeline.playback_loop_deadline();
                    pipeline_services
                        .wait
                        .wait_poll_interval_and_delay_scheduler_until(
                            &mut pipeline.scheduler,
                            playback_loop_deadline,
                        );
                }
                continue;
            }

            let tick_status = service_playback_tick(PlaybackTickContext {
                session_id: session.id(),
                demux_cache: &demux_cache,
                http_cache: http_cache.as_ref(),
                services: &mut pipeline_services,
                pipeline: &mut pipeline,
                control: &control,
                event_tx: &event_tx,
                vo_queue: &video_output_queue,
                frame_presented: &frame_presented,
            })?;
            if matches!(tick_status, PlaybackTickStatus::ForceRebufferAudioRealign) {
                if service_rebuffer_audio_realign_seek_if_needed(
                    &mut session,
                    &control,
                    &demux_cache,
                    &mut pipeline,
                    &video_output_queue,
                    &event_tx,
                    emit_playback_buffered_events,
                )? {
                    continue;
                }
                tracing::debug!(
                    session_id = ?session.id(),
                    "playback tick requested rebuffer audio realign without pending request"
                );
                continue;
            }
            if let PlaybackTickStatus::RecoveryPending(request) = tick_status {
                if service_cached_seek_recovery_fallback_if_needed(
                    &mut session,
                    &control,
                    &demux_cache,
                    &mut pipeline,
                    &video_output_queue,
                    &event_tx,
                    emit_playback_buffered_events,
                    Some(request),
                )? {
                    log_recovery_request_miss_summary(
                        &mut missing_recovery_request_tracker,
                        session.id(),
                    );
                    continue;
                }
                wait_after_missing_recovery_request(
                    &mut pipeline,
                    &pipeline_services.wait,
                    &mut missing_recovery_request_tracker,
                    request,
                    session.id(),
                    "playback_tick",
                );
                continue;
            }
            if service_cached_seek_recovery_fallback_if_needed(
                &mut session,
                &control,
                &demux_cache,
                &mut pipeline,
                &video_output_queue,
                &event_tx,
                emit_playback_buffered_events,
                None,
            )? {
                log_recovery_request_miss_summary(
                    &mut missing_recovery_request_tracker,
                    session.id(),
                );
                continue;
            }
            match tick_status {
                PlaybackTickStatus::Continue => continue,
                PlaybackTickStatus::RecoveryPending(_) => continue,
                PlaybackTickStatus::ForceRebufferAudioRealign => continue,
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

fn rebuffer_audio_realign_requires_low_level_seek(
    _attempts: u8,
    _queued_video_covers_target: bool,
) -> bool {
    false
}

fn rebuffer_audio_realign_can_preserve_video_queue(
    attempts: u8,
    queued_video_covers_target: bool,
    audio_stream_available: bool,
) -> bool {
    audio_stream_available && attempts == 1 && queued_video_covers_target
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioRealignExecutionDecision {
    Execute,
    CoverageSatisfied,
    InputPending,
}

impl AudioRealignExecutionDecision {
    fn as_str(self) -> &'static str {
        match self {
            Self::Execute => "execute",
            Self::CoverageSatisfied => "coverage_satisfied",
            Self::InputPending => "input_pending",
        }
    }
}

fn audio_realign_execution_decision(
    target_timeline_nsecs: u64,
    pending_coverage: AudioRealignCoverage,
    audio_output_range_nsecs: Option<(u64, u64)>,
    in_flight_packets: usize,
) -> (AudioRealignExecutionDecision, Option<u64>) {
    let audio_output_coverage_nsecs = audio_output_range_nsecs.and_then(
        |(played_timeline_nsecs, buffered_until_timeline_nsecs)| {
            let accepted_start_limit_nsecs = target_timeline_nsecs
                .saturating_add(duration_nsecs(VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE));
            (played_timeline_nsecs <= accepted_start_limit_nsecs
                && buffered_until_timeline_nsecs > target_timeline_nsecs)
                .then(|| {
                    buffered_until_timeline_nsecs
                        .saturating_sub(played_timeline_nsecs.max(target_timeline_nsecs))
                })
        },
    );
    if pending_coverage.ready
        || audio_output_coverage_nsecs
            .is_some_and(|coverage| coverage >= pending_coverage.protected_target_nsecs)
    {
        return (
            AudioRealignExecutionDecision::CoverageSatisfied,
            audio_output_coverage_nsecs,
        );
    }
    if in_flight_packets > 0 {
        return (
            AudioRealignExecutionDecision::InputPending,
            audio_output_coverage_nsecs,
        );
    }
    (
        AudioRealignExecutionDecision::Execute,
        audio_output_coverage_nsecs,
    )
}

fn internal_recovery_seek_buffering_policy(
    output_snapshot: PlaybackOutputSnapshot,
) -> PlaybackSeekBufferingPolicy {
    let can_preserve_visible_frame = !output_snapshot.first_video_frame_pending
        && !output_snapshot.rebuffering
        && !output_snapshot.video_output_low_water
        && !output_snapshot.video_decode_underfill
        && output_snapshot.queued_video_frames > 0;
    if can_preserve_visible_frame {
        PlaybackSeekBufferingPolicy::PreserveVisibleFrame
    } else {
        PlaybackSeekBufferingPolicy::Emit
    }
}

fn service_rebuffer_audio_realign_seek_if_needed(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    demux_cache: &DemuxPacketCache,
    pipeline: &mut PlaybackPipelineState,
    vo_queue: &VideoOutputQueue,
    event_tx: &Sender<BackendEvent>,
    emit_playback_buffered_events: bool,
) -> std::result::Result<bool, String> {
    let Some(request) = pipeline
        .output_scheduler
        .take_rebuffer_audio_realign_request()
    else {
        return Ok(false);
    };
    let pending_coverage = pipeline.output_scheduler.audio_realign_coverage(
        request.target_timeline_nsecs,
        duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION),
    );
    let audio_output_snapshot = pipeline
        .audio_output
        .as_ref()
        .and_then(|output| output.snapshot().ok());
    let audio_decode_snapshot = pipeline
        .audio_decode_pipeline
        .as_ref()
        .map(AudioDecodePipeline::snapshot);
    let retained_far_ahead_frame = pipeline
        .audio_decode_pipeline
        .as_ref()
        .is_some_and(AudioDecodePipeline::has_deferred_output_frame);
    let (arbitrated_execution_decision, audio_output_coverage_nsecs) =
        audio_realign_execution_decision(
            request.target_timeline_nsecs,
            pending_coverage,
            audio_output_snapshot.map(|snapshot| {
                (
                    snapshot.played_timeline_nsecs,
                    snapshot.buffered_until_timeline_nsecs,
                )
            }),
            if retained_far_ahead_frame {
                0
            } else {
                audio_decode_snapshot
                    .map(|snapshot| snapshot.in_flight_packets)
                    .unwrap_or_default()
            },
        );
    let execution_decision = if retained_far_ahead_frame {
        AudioRealignExecutionDecision::Execute
    } else {
        arbitrated_execution_decision
    };
    if execution_decision != AudioRealignExecutionDecision::Execute {
        if execution_decision == AudioRealignExecutionDecision::InputPending {
            pipeline
                .output_scheduler
                .defer_audio_reader_gap_watchdog_after_input_pending(request.target_timeline_nsecs);
        }
        tracing::debug!(
            session_id = ?session.id(),
            transaction_id = ?pipeline
                .audio_realign_transaction
                .map(|transaction| transaction.transaction_id),
            recovery_scope = "audio_realign",
            target_timeline_nsecs = request.target_timeline_nsecs,
            reason = request.reason,
            arbitration_outcome = execution_decision.as_str(),
            audio_accepted_start = ?pending_coverage.audio_accepted_start_timeline_nsecs,
            start_gap_ms = ?pending_coverage
                .start_gap_nsecs
                .map(|gap| gap as f64 / 1_000_000.0),
            contiguous_coverage_ms = ?pending_coverage
                .contiguous_coverage_nsecs
                .map(|coverage| coverage as f64 / 1_000_000.0),
            audio_output_coverage_ms = ?audio_output_coverage_nsecs
                .map(|coverage| coverage as f64 / 1_000_000.0),
            coverage_target_ms = pending_coverage.protected_target_nsecs as f64 / 1_000_000.0,
            audio_decode_pending_input_packets = ?audio_decode_snapshot
                .map(|snapshot| snapshot.pending_input_packets),
            audio_decode_in_flight_packets = ?audio_decode_snapshot
                .map(|snapshot| snapshot.in_flight_packets),
            retained_far_ahead_frame,
            "discarded queued FFmpeg audio realign after live-state recheck"
        );
        return Ok(true);
    }
    match pipeline.observe_rebuffer_audio_realign_request(request) {
        AudioRealignRequestAction::Start => service_rebuffer_audio_realign_request(
            session,
            control,
            demux_cache,
            pipeline,
            vo_queue,
            event_tx,
            emit_playback_buffered_events,
            request,
            1,
            false,
        ),
        AudioRealignRequestAction::Coalesce {
            transaction,
            reason,
        } => {
            let worker = pipeline
                .audio_decode_pipeline
                .as_ref()
                .map(AudioDecodePipeline::snapshot);
            tracing::debug!(
                session_id = ?session.id(),
                transaction_id = transaction.transaction_id,
                recovery_scope = "audio_realign",
                target_timeline_nsecs = request.target_timeline_nsecs,
                transaction_generation = transaction.generation,
                transaction_elapsed_ms = transaction.started_at.elapsed().as_secs_f64() * 1000.0,
                attempts = transaction.attempts,
                transaction_phase = transaction.phase.as_str(),
                coverage_ms = transaction.coverage_nsecs as f64 / 1_000_000.0,
                coverage_target_ms = transaction.coverage_target_nsecs as f64 / 1_000_000.0,
                recovery_satisfied = transaction.phase
                    == super::playback_pipeline_state::AudioRealignPhase::Covered,
                fallback_eligible = false,
                coalesce_reason = reason.as_str(),
                recovery_generation = ?worker.and_then(|snapshot| snapshot.recovery_generation),
                recovery_elapsed_ms = ?worker
                    .and_then(|snapshot| snapshot.recovery_elapsed)
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                flush_command_sent = ?worker.map(|snapshot| snapshot.flush_command_sent),
                in_flight_packets = ?worker.map(|snapshot| snapshot.in_flight_packets),
                stale_results_discarded = ?worker
                    .map(|snapshot| snapshot.stale_results_discarded),
                last_result_progress_ms = ?worker
                    .and_then(|snapshot| snapshot.last_result_progress_elapsed)
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                "coalesced repeated FFmpeg rebuffer audio realign request"
            );
            Ok(true)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn service_rebuffer_audio_realign_request(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    demux_cache: &DemuxPacketCache,
    pipeline: &mut PlaybackPipelineState,
    vo_queue: &VideoOutputQueue,
    event_tx: &Sender<BackendEvent>,
    emit_playback_buffered_events: bool,
    request: RebufferAudioRealignRequest,
    attempts: u8,
    force_low_level_fallback: bool,
) -> std::result::Result<bool, String> {
    let position_seconds = nsecs_to_seconds(request.target_timeline_nsecs);
    let audio_stream_index = pipeline.audio_stream.map(|stream| stream.index);
    let output_snapshot = pipeline.output_scheduler.snapshot();
    let audio_output_snapshot = pipeline
        .audio_output
        .as_ref()
        .and_then(|output| output.snapshot().ok());
    let queued_video_covers_target =
        output_snapshot
            .queued_video_range_nsecs
            .is_some_and(|(start, end)| {
                start <= request.target_timeline_nsecs && request.target_timeline_nsecs < end
            });
    let mut force_low_level_seek = force_low_level_fallback
        || rebuffer_audio_realign_requires_low_level_seek(attempts, queued_video_covers_target);
    let can_preserve_video_queue = !force_low_level_seek
        && rebuffer_audio_realign_can_preserve_video_queue(
            attempts,
            queued_video_covers_target,
            audio_stream_index.is_some() && pipeline.audio_decode_pipeline.is_some(),
        );
    let first_video_after_anchor_gap_ms = (i128::from(request.first_video_timeline_nsecs)
        - i128::from(request.anchor_timeline_nsecs))
        as f64
        / 1_000_000.0;
    let far_ahead_audio_delta_ms = (i128::from(request.far_ahead_audio_timeline_nsecs)
        - i128::from(request.target_timeline_nsecs)) as f64
        / 1_000_000.0;
    tracing::debug!(
        session_id = ?session.id(),
        position_seconds,
        target_timeline_nsecs = request.target_timeline_nsecs,
        reason = request.reason,
        anchor_timeline_nsecs = request.anchor_timeline_nsecs,
        first_video_timeline_nsecs = request.first_video_timeline_nsecs,
        first_video_after_anchor_gap_ms,
        far_ahead_audio_timeline_nsecs = request.far_ahead_audio_timeline_nsecs,
        far_ahead_audio_delta_ms,
        far_ahead_observation_count = request.far_ahead_observation_count,
        attempts,
        force_low_level_seek,
        force_low_level_fallback,
        can_preserve_video_queue,
        audio_stream_index = ?audio_stream_index,
        queued_video_frames = output_snapshot.queued_video_frames,
        queued_video_ms = output_snapshot.queued_video_duration_nsecs as f64 / 1_000_000.0,
        queued_video_range = ?output_snapshot.queued_video_range_nsecs,
        queued_video_covers_target,
        queued_video_forward_ms = ?output_snapshot
            .queued_video_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        queued_video_contiguous_forward_ms = ?output_snapshot
            .queued_video_contiguous_forward_nsecs
            .map(|duration| duration as f64 / 1_000_000.0),
        queued_video_largest_gap_ms = ?output_snapshot
            .queued_video_largest_gap_nsecs
            .map(|gap| gap as f64 / 1_000_000.0),
        output_state = ?output_snapshot.state,
        output_first_video_frame_pending = output_snapshot.first_video_frame_pending,
        output_rebuffering = output_snapshot.rebuffering,
        output_rebuffer_anchor = ?output_snapshot.video_output_rebuffer_anchor,
        audio_output_pending_ms = ?audio_output_snapshot
            .map(|snapshot| snapshot.total_pending_nsecs as f64 / 1_000_000.0),
        audio_output_queue_ms = ?audio_output_snapshot
            .map(|snapshot| snapshot.queue_pending_nsecs as f64 / 1_000_000.0),
        pending_start_audio_ms = output_snapshot.pending_start_audio_nsecs as f64 / 1_000_000.0,
        "evaluating FFmpeg rebuffer audio realign recovery path"
    );

    pipeline.retain_audio_for_realign(session.id(), request.reason);

    if can_preserve_video_queue && let Some(audio_stream_index) = audio_stream_index {
        let audio_realign_requires_recovery_point = pipeline
            .audio_stream
            .is_some_and(|stream| audio_codec_requires_recovery_point(stream.codec_id));
        let reader_realign = demux_cache.realign_stream_reader_to_timeline(
            audio_stream_index,
            request.target_timeline_nsecs,
            request.reason,
        );
        if reader_realign.is_none()
            && (!queued_video_covers_target || audio_realign_requires_recovery_point)
        {
            force_low_level_seek |= audio_realign_requires_recovery_point;
            tracing::debug!(
                session_id = ?session.id(),
                target_timeline_nsecs = request.target_timeline_nsecs,
                attempts,
                queued_video_covers_target,
                audio_stream_index,
                audio_realign_requires_recovery_point,
                force_low_level_seek,
                "FFmpeg rebuffer audio realign reader reposition unavailable"
            );
        } else {
            let recovery_started_at = Instant::now();
            let generation = pipeline.advance_playback_generation();
            if let Some(audio_decode_pipeline) = pipeline.audio_decode_pipeline.as_mut() {
                audio_decode_pipeline.flush_buffers(generation)?;
            }
            pipeline.audio_clock = TimestampMapper::new(
                pipeline.audio_stream.and_then(|stream| stream.start_nsecs),
                request.target_timeline_nsecs,
                None,
            );
            if let Some(audio_output) = pipeline.audio_output.as_ref() {
                audio_output.reset_clock(request.target_timeline_nsecs);
            }
            pipeline
                .output_scheduler
                .prepare_audio_after_rebuffer_realign(
                    request.target_timeline_nsecs,
                    session.id(),
                    request.reason,
                );
            let transaction_id = pipeline.begin_recovery_transaction();
            pipeline.begin_audio_realign_transaction(
                transaction_id,
                request,
                generation,
                recovery_started_at,
            );
            control.set_cache_paused(false);
            tracing::debug!(
                session_id = ?session.id(),
                transaction_id,
                recovery_scope = "audio_realign",
                target_timeline_nsecs = request.target_timeline_nsecs,
                reason = request.reason,
                anchor_timeline_nsecs = request.anchor_timeline_nsecs,
                first_video_timeline_nsecs = request.first_video_timeline_nsecs,
                first_video_after_anchor_gap_ms,
                far_ahead_audio_timeline_nsecs = request.far_ahead_audio_timeline_nsecs,
                far_ahead_audio_delta_ms,
                far_ahead_observation_count = request.far_ahead_observation_count,
                attempts,
                queued_video_frames = output_snapshot.queued_video_frames,
                queued_video_ms = output_snapshot.queued_video_duration_nsecs as f64 / 1_000_000.0,
                queued_video_range = ?output_snapshot.queued_video_range_nsecs,
                queued_video_covers_target,
                audio_stream_index,
                reader_realign = ?reader_realign,
                playback_generation = generation,
                "handled FFmpeg rebuffer audio realign while preserving video queue"
            );
            return Ok(true);
        }
    }

    control.set_cache_paused(false);
    let start_audio_realign_transaction = pipeline.audio_realign_transaction.is_none();
    let audio_recovery_transaction_id = pipeline
        .audio_realign_transaction
        .map(|transaction| transaction.transaction_id);
    let recovery_started_at = Instant::now();
    let seek_generation = control.request_seek();
    session.reset_to(session.id(), position_seconds);
    pipeline.current_start_position_nsecs = session.start_position_nsecs();
    tracing::debug!(
        session_id = ?session.id(),
        transaction_id = ?audio_recovery_transaction_id,
        recovery_scope = "audio_realign",
        position_seconds,
        target_timeline_nsecs = request.target_timeline_nsecs,
        reason = request.reason,
        anchor_timeline_nsecs = request.anchor_timeline_nsecs,
        first_video_timeline_nsecs = request.first_video_timeline_nsecs,
        first_video_after_anchor_gap_ms,
        far_ahead_audio_timeline_nsecs = request.far_ahead_audio_timeline_nsecs,
        far_ahead_audio_delta_ms,
        far_ahead_observation_count = request.far_ahead_observation_count,
        attempts,
        force_low_level_seek,
        can_preserve_video_queue,
        seek_generation,
        audio_stream_index = ?audio_stream_index,
        queued_video_frames = output_snapshot.queued_video_frames,
        queued_video_ms = output_snapshot.queued_video_duration_nsecs as f64 / 1_000_000.0,
        queued_video_range = ?output_snapshot.queued_video_range_nsecs,
        queued_video_covers_target,
        audio_output_pending_ms = ?audio_output_snapshot
            .map(|snapshot| snapshot.total_pending_nsecs as f64 / 1_000_000.0),
        "handling FFmpeg rebuffer audio realign with playback seek reset"
    );
    let demux_seek_result = service_playback_seek_reset(PlaybackSeekResetContext {
        position_seconds,
        seek_mode: crate::player::backend::PlaybackSeekMode::Precise,
        seek_generation,
        force_low_level_seek,
        cache_only: false,
        require_safe_cached_anchor: false,
        preserve_hevc_same_hardware_recovery: false,
        recovery_transaction_id: audio_recovery_transaction_id,
        low_level_seek_reason: force_low_level_seek.then_some(request.reason),
        session_id: session.id(),
        vo_queue,
        demux_cache,
        pipeline,
        emit_playback_buffered_events,
        buffering_policy: internal_recovery_seek_buffering_policy(output_snapshot),
        control,
        event_tx,
    })?;
    let recovery_generation = pipeline.playback_generation.current();
    if start_audio_realign_transaction {
        let transaction_id = pipeline.active_recovery_transaction_id();
        pipeline.begin_audio_realign_transaction(
            transaction_id,
            request,
            recovery_generation,
            recovery_started_at,
        );
    } else {
        pipeline.update_audio_realign_recovery_generation(recovery_generation);
    }
    let transaction_id = pipeline
        .audio_realign_transaction
        .map(|transaction| transaction.transaction_id);
    tracing::debug!(
        session_id = ?session.id(),
        transaction_id = ?transaction_id,
        recovery_scope = "audio_realign",
        position_seconds,
        target_timeline_nsecs = request.target_timeline_nsecs,
        reason = request.reason,
        attempts,
        force_low_level_seek,
        seek_generation,
        recovery_generation,
        ?demux_seek_result,
        "handled FFmpeg rebuffer audio realign with playback seek reset"
    );
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn service_audio_realign_recovery_watchdog_if_needed(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    demux_cache: &DemuxPacketCache,
    pipeline: &mut PlaybackPipelineState,
    vo_queue: &VideoOutputQueue,
    event_tx: &Sender<BackendEvent>,
    emit_playback_buffered_events: bool,
) -> std::result::Result<bool, String> {
    if let Some(audio_decode_pipeline) = pipeline.audio_decode_pipeline.as_mut() {
        audio_decode_pipeline.service_worker()?;
    }
    if let Some(transaction) = pipeline.clear_audio_realign_transaction_after_resume(session.id()) {
        tracing::debug!(
            session_id = ?session.id(),
            transaction_id = transaction.transaction_id,
            recovery_scope = "audio_realign",
            target_timeline_nsecs = transaction.target_timeline_nsecs,
            transaction_generation = transaction.generation,
            transaction_elapsed_ms = transaction.started_at.elapsed().as_secs_f64() * 1000.0,
            attempts = transaction.attempts,
            transaction_phase = transaction.phase.as_str(),
            coverage_ms = transaction.coverage_nsecs as f64 / 1_000_000.0,
            coverage_target_ms = transaction.coverage_target_nsecs as f64 / 1_000_000.0,
            "cleared FFmpeg audio realign transaction after contiguous playback resumed"
        );
        return Ok(false);
    }
    let Some(action) = pipeline.poll_audio_recovery_watchdog() else {
        return Ok(false);
    };
    match action {
        AudioRecoveryWatchdogAction::Warn {
            transaction,
            worker,
        } => {
            tracing::warn!(
                session_id = ?session.id(),
                transaction_id = transaction.transaction_id,
                recovery_scope = "audio_realign",
                target_timeline_nsecs = transaction.target_timeline_nsecs,
                transaction_generation = transaction.generation,
                transaction_elapsed_ms = transaction.started_at.elapsed().as_secs_f64() * 1000.0,
                attempts = transaction.attempts,
                transaction_phase = transaction.phase.as_str(),
                coverage_ms = transaction.coverage_nsecs as f64 / 1_000_000.0,
                coverage_target_ms = transaction.coverage_target_nsecs as f64 / 1_000_000.0,
                fallback_eligible = false,
                recovery_generation = ?worker.recovery_generation,
                recovery_elapsed_ms = ?worker
                    .recovery_elapsed
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                flush_command_sent = worker.flush_command_sent,
                in_flight_packets = worker.in_flight_packets,
                stale_results_discarded = worker.stale_results_discarded,
                last_result_progress_ms = ?worker
                    .last_result_progress_elapsed
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                "FFmpeg audio decoder recovery has made no progress for 500ms"
            );
            Ok(false)
        }
        AudioRecoveryWatchdogAction::LowLevelFallback {
            transaction,
            worker,
            request,
        } => {
            tracing::warn!(
                session_id = ?session.id(),
                transaction_id = transaction.transaction_id,
                recovery_scope = "audio_realign",
                target_timeline_nsecs = transaction.target_timeline_nsecs,
                transaction_generation = transaction.generation,
                transaction_elapsed_ms = transaction.started_at.elapsed().as_secs_f64() * 1000.0,
                attempts = transaction.attempts,
                transaction_phase = transaction.phase.as_str(),
                coverage_ms = transaction.coverage_nsecs as f64 / 1_000_000.0,
                coverage_target_ms = transaction.coverage_target_nsecs as f64 / 1_000_000.0,
                fallback_eligible = true,
                recovery_generation = ?worker.recovery_generation,
                recovery_elapsed_ms = ?worker
                    .recovery_elapsed
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                flush_command_sent = worker.flush_command_sent,
                in_flight_packets = worker.in_flight_packets,
                stale_results_discarded = worker.stale_results_discarded,
                last_result_progress_ms = ?worker
                    .last_result_progress_elapsed
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                fallback = "single_low_level_seek",
                arbitration_outcome = "watchdog_low_level_fallback",
                "FFmpeg audio decoder recovery timed out; executing bounded low-level fallback"
            );
            service_rebuffer_audio_realign_request(
                session,
                control,
                demux_cache,
                pipeline,
                vo_queue,
                event_tx,
                emit_playback_buffered_events,
                request,
                transaction.attempts,
                true,
            )
        }
        AudioRecoveryWatchdogAction::FallbackExhausted {
            transaction,
            worker,
        } => {
            tracing::error!(
                session_id = ?session.id(),
                transaction_id = transaction.transaction_id,
                recovery_scope = "audio_realign",
                target_timeline_nsecs = transaction.target_timeline_nsecs,
                transaction_generation = transaction.generation,
                transaction_elapsed_ms = transaction.started_at.elapsed().as_secs_f64() * 1000.0,
                attempts = transaction.attempts,
                transaction_phase = transaction.phase.as_str(),
                coverage_ms = transaction.coverage_nsecs as f64 / 1_000_000.0,
                coverage_target_ms = transaction.coverage_target_nsecs as f64 / 1_000_000.0,
                fallback_eligible = false,
                recovery_generation = ?worker.recovery_generation,
                recovery_elapsed_ms = ?worker
                    .recovery_elapsed
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                flush_command_sent = worker.flush_command_sent,
                in_flight_packets = worker.in_flight_packets,
                stale_results_discarded = worker.stale_results_discarded,
                last_result_progress_ms = ?worker
                    .last_result_progress_elapsed
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.0),
                fallback_suppressed = true,
                "FFmpeg audio decoder recovery remained stalled after bounded fallback"
            );
            let Some((terminal_transaction, resume_timeline_nsecs)) =
                pipeline.finish_audio_realign_as_confirmed_media_gap(control, session.id())
            else {
                return Err(
                    "FFmpeg audio realign fallback exhausted without an active transaction"
                        .to_string(),
                );
            };
            tracing::warn!(
                session_id = ?session.id(),
                transaction_id = terminal_transaction.transaction_id,
                recovery_scope = "audio_realign",
                target_timeline_nsecs = terminal_transaction.target_timeline_nsecs,
                resume_timeline_nsecs,
                far_ahead_audio_timeline_nsecs =
                    terminal_transaction.request.far_ahead_audio_timeline_nsecs,
                transaction_elapsed_ms =
                    terminal_transaction.started_at.elapsed().as_secs_f64() * 1000.0,
                attempts = terminal_transaction.attempts,
                transaction_phase = terminal_transaction.phase.as_str(),
                "committed bounded FFmpeg audio realign as a confirmed media gap"
            );
            Ok(true)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn service_hevc_same_hardware_recovery_if_needed(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    demux_cache: &DemuxPacketCache,
    pipeline: &mut PlaybackPipelineState,
    vo_queue: &VideoOutputQueue,
    event_tx: &Sender<BackendEvent>,
    emit_playback_buffered_events: bool,
    playback_wait: &PlaybackPipelineWaitService,
    decode_pipeline: &mut DecodePipelineService,
    frame_presented: &AtomicBool,
) -> std::result::Result<bool, String> {
    let now = Instant::now();
    if let Some(drop) = pipeline
        .output_scheduler
        .take_decode_recovery_drop_for_fallback()
    {
        if pipeline
            .video_decode_pipeline
            .hevc_same_hardware_recovery_target()
            .is_none()
        {
            return Err(format!(
                "{} recovery produced an unbridged continuous decode gap of {:.3}s at {:.3}s (first frame {:.3}s) after bounded decoder fallback was exhausted",
                drop.source.as_str(),
                drop.gap_nsecs as f64 / 1_000_000_000.0,
                drop.target_nsecs as f64 / 1_000_000_000.0,
                drop.first_frame_nsecs as f64 / 1_000_000_000.0,
            ));
        }
        pipeline
            .video_decode_pipeline
            .mark_hevc_same_hardware_unbridged_continuous_gap();
        tracing::warn!(
            session_id = ?session.id(),
            transaction_id = drop.transaction_id,
            attempt_id = ?pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_attempt_id(),
            decoder_epoch = ?pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_decoder_epoch(),
            recovery_source = drop.source.as_str(),
            target_nsecs = drop.target_nsecs,
            first_frame_nsecs = drop.first_frame_nsecs,
            gap_ms = drop.gap_nsecs as f64 / 1_000_000.0,
            "routed unbridged continuous decode gap back into bounded decoder fallback"
        );
    }
    let action = pipeline
        .video_decode_pipeline
        .pending_hevc_same_hardware_recovery_action(now);

    if action == HevcDecodeRecoveryAction::None {
        let Some(ticket) = pipeline
            .video_decode_pipeline
            .hevc_same_hardware_prewarm_ticket()
        else {
            return Ok(false);
        };
        match vo_queue.vulkan_prewarm_status(ticket) {
            VulkanPrewarmStatus::Pending => {
                let deadline = pipeline.playback_loop_deadline();
                playback_wait.wait_poll_interval_and_delay_scheduler_until(
                    &mut pipeline.scheduler,
                    deadline,
                );
                return Ok(true);
            }
            VulkanPrewarmStatus::Ready => {
                let target_nsecs = pipeline
                    .video_decode_pipeline
                    .hevc_same_hardware_recovery_target()
                    .ok_or_else(|| {
                        "Vulkan prewarm completed without a same-hardware target".to_string()
                    })?;
                if !pipeline
                    .video_decode_pipeline
                    .mark_hevc_same_hardware_prewarm_ready(now)
                {
                    return Err(
                        "Vulkan prewarm completed outside same-hardware recovery".to_string()
                    );
                }
                let replay_packets = pipeline
                    .video_decode_pipeline
                    .requeue_hevc_hw_replay_journal(
                        &mut pipeline.playback_generation,
                        target_nsecs,
                        session.id(),
                    )?;
                pipeline
                    .video_decode_pipeline
                    .record_hevc_same_hardware_replay(replay_packets, true, now);
                if replay_packets > 0 {
                    pipeline.dovi_pipeline.reset();
                    pipeline
                        .video_decode_recovery
                        .begin_verified_replay_from_safe_anchor(
                            pipeline.video_stream.codec_id,
                            target_nsecs,
                        );
                    pipeline
                        .output_scheduler
                        .mark_decode_recovery_replaying(pipeline.active_recovery_transaction_id());
                    pipeline.begin_cached_seek_recovery_watchdog(target_nsecs, session.id());
                }
                tracing::info!(
                    session_id = ?session.id(),
                    target_nsecs,
                    recovery_action = HevcDecodeRecoveryAction::ReplaySameHardware.as_str(),
                    replay_packets,
                    vulkan_device = ticket.device_key(),
                    same_hw_reopen_result = "prewarm_ready",
                    "replayed safe HEVC journal after same-Vulkan renderer prewarm"
                );
                return Ok(true);
            }
            VulkanPrewarmStatus::Failed(error) => {
                pipeline
                    .video_decode_pipeline
                    .fail_hevc_same_hardware_recovery(format!(
                        "Vulkan renderer prewarm failed: {error}"
                    ));
                return Ok(true);
            }
            VulkanPrewarmStatus::Stale => {
                pipeline
                    .video_decode_pipeline
                    .fail_hevc_same_hardware_recovery(
                        "Vulkan renderer prewarm request became stale before replay",
                    );
                return Ok(true);
            }
        }
    }

    if let Some(suppressed_repeats) = pipeline
        .video_decode_pipeline
        .hevc_same_hardware_action_log_summary(action, now)
    {
        tracing::debug!(
            session_id = ?session.id(),
            transaction_id = pipeline.active_recovery_transaction_id(),
            attempt_id = ?pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_attempt_id(),
            decoder_epoch = ?pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_decoder_epoch(),
            recovery_action = action.as_str(),
            target_nsecs = ?pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_target(),
            requested_hw_mode = ?pipeline.video_decode_pipeline.requested_hardware_mode(),
            suppressed_repeats,
            "servicing bounded HEVC same-Vulkan recovery action"
        );
    }

    match action {
        HevcDecodeRecoveryAction::None => Ok(false),
        HevcDecodeRecoveryAction::DrainPendingResults => {
            let before = pipeline.video_decode_pipeline.snapshot();
            let max_passes = before
                .command_queue_capacity
                .saturating_add(before.queue_capacity)
                .saturating_add(1)
                .max(1);
            let mut made_progress = false;
            let mut passes = 0usize;
            while passes < max_passes {
                passes = passes.saturating_add(1);
                let status = decode_pipeline.service_once(DecodePipelineServiceContext {
                    pipeline,
                    control,
                    session_id: session.id(),
                    event_tx,
                    vo_queue,
                    frame_presented,
                    demux_reader_watermark: || demux_cache.cached_reader_watermark(),
                })?;
                made_progress |= status.made_progress();
                if status.interrupted() || !status.made_progress() {
                    break;
                }
            }
            let after = pipeline.video_decode_pipeline.snapshot();
            // mpv's lavc_process() receive-first loop only treats work from the
            // video decoder as decoder progress. Audio/subtitle output drained
            // by service_once must not keep a failed video decoder in grace.
            let video_result_progress = hevc_drain_video_result_progressed(before, after);
            made_progress |= video_result_progress;
            let advanced = pipeline
                .video_decode_pipeline
                .record_hevc_same_hardware_drain_pass(video_result_progress, Instant::now());
            let drain_now = Instant::now();
            if let Some(suppressed_repeats) = pipeline
                .video_decode_pipeline
                .hevc_same_hardware_drain_log_summary(advanced, drain_now)
            {
                tracing::debug!(
                    session_id = ?session.id(),
                    transaction_id = pipeline.active_recovery_transaction_id(),
                    attempt_id = ?pipeline
                        .video_decode_pipeline
                        .hevc_same_hardware_recovery_attempt_id(),
                    decoder_epoch = ?pipeline
                        .video_decode_pipeline
                        .hevc_same_hardware_recovery_decoder_epoch(),
                    passes,
                    made_progress,
                    video_result_progress,
                    advanced,
                    suppressed_repeats,
                    submitted_sequence = after.submitted_sequence,
                    result_produced_sequence = after.result_produced_sequence,
                    result_consumed_sequence = after.result_consumed_sequence,
                    submitted_not_consumed_packets = after.submitted_not_consumed_packets,
                    last_worker_progress_ms = ?after.last_result_produced_at.map(|at| {
                        drain_now.saturating_duration_since(at).as_secs_f64() * 1000.0
                    }),
                    "drained pending decode output for same-Vulkan recovery"
                );
            }
            if !advanced {
                let deadline = pipeline.playback_loop_deadline();
                playback_wait.wait_poll_interval_and_delay_scheduler_until(
                    &mut pipeline.scheduler,
                    deadline,
                );
            }
            Ok(true)
        }
        HevcDecodeRecoveryAction::FlushSameHardware => {
            let target_nsecs = pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_target()
                .ok_or_else(|| "same-Vulkan flush has no recovery target".to_string())?;
            control.set_cache_paused(false);
            let discarded_vo_frames = vo_queue.discard_pending_frames(session.id());
            let generation = pipeline.advance_playback_generation();
            pipeline
                .video_frame_prepare_worker
                .flush_generation(generation);
            pipeline.output_scheduler.begin_decode_recovery(
                pipeline.active_recovery_transaction_id(),
                target_nsecs,
                DecodeRecoverySource::FlushReplay,
                control,
                session.id(),
            );
            pipeline
                .video_decode_recovery
                .reset_for_timeline_start(pipeline.video_stream.codec_id, target_nsecs);
            if let Err(error) = pipeline
                .video_decode_pipeline
                .begin_hevc_same_hardware_flush(generation, now)
            {
                tracing::warn!(
                    session_id = ?session.id(),
                    target_nsecs,
                    %error,
                    same_hw_recovery_phase = "flush_failed",
                    "same-Vulkan decoder flush could not be scheduled"
                );
                return Ok(true);
            }
            pipeline
                .video_decode_pipeline
                .reset_hevc_decoder_transient_preserving_gap_evidence(now);
            let replay_packets = pipeline
                .video_decode_pipeline
                .requeue_hevc_hw_replay_journal(
                    &mut pipeline.playback_generation,
                    target_nsecs,
                    session.id(),
                )?;
            pipeline
                .video_decode_pipeline
                .record_hevc_same_hardware_replay(replay_packets, false, now);
            if replay_packets > 0 {
                pipeline.dovi_pipeline.reset();
                pipeline
                    .video_decode_recovery
                    .begin_verified_replay_from_safe_anchor(
                        pipeline.video_stream.codec_id,
                        target_nsecs,
                    );
                pipeline
                    .output_scheduler
                    .mark_decode_recovery_replaying(pipeline.active_recovery_transaction_id());
                pipeline.begin_cached_seek_recovery_watchdog(target_nsecs, session.id());
            }
            tracing::info!(
                session_id = ?session.id(),
                target_nsecs,
                discarded_vo_frames,
                replay_packets,
                recovery_action = HevcDecodeRecoveryAction::ReplaySameHardware.as_str(),
                same_hw_recovery_phase = "replaying_after_flush",
                "flushed and replayed the current Vulkan decoder"
            );
            Ok(true)
        }
        HevcDecodeRecoveryAction::ReopenSameHardware => {
            let target_nsecs = pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_target()
                .ok_or_else(|| "same-Vulkan reopen has no recovery target".to_string())?;
            let generation = pipeline.advance_playback_generation();
            let release_first = pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_is_resource_pressure();
            let mut discarded_vo_frames = 0;
            let mut released_scheduler_frames = 0;
            if release_first {
                control.set_cache_paused(false);
                released_scheduler_frames = pipeline
                    .output_scheduler
                    .release_vulkan_frames_for_resource_pressure(control, session.id());
                discarded_vo_frames = vo_queue.discard_pending_frames(session.id());
                if let Err(error) = pipeline
                    .video_frame_prepare_worker
                    .restart_after_resource_pressure(generation)
                {
                    pipeline
                        .video_decode_pipeline
                        .fail_hevc_same_hardware_recovery(format!(
                            "same-Vulkan reopen could not retire frame-prepare worker: {error}"
                        ));
                    tracing::error!(
                        session_id = ?session.id(),
                        target_nsecs,
                        generation,
                        released_scheduler_frames,
                        discarded_vo_frames,
                        %error,
                        "aborted release-first Vulkan reopen before opening a second frame pool"
                    );
                    return Ok(true);
                }
                pipeline.output_scheduler.begin_decode_recovery(
                    pipeline.active_recovery_transaction_id(),
                    target_nsecs,
                    DecodeRecoverySource::VulkanReopenReplay,
                    control,
                    session.id(),
                );
                pipeline
                    .video_decode_recovery
                    .reset_for_timeline_start(pipeline.video_stream.codec_id, target_nsecs);
            }
            let device = match pipeline
                .video_decode_pipeline
                .begin_hevc_same_hardware_reopen(pipeline.video_stream, generation, now)
            {
                Ok(device) => device,
                Err(error) => {
                    tracing::warn!(
                        session_id = ?session.id(),
                        target_nsecs,
                        %error,
                        same_hw_reopen_result = "failed",
                        "same-Vulkan decoder reopen failed"
                    );
                    return Ok(true);
                }
            };
            if !release_first {
                control.set_cache_paused(false);
                discarded_vo_frames = vo_queue.discard_pending_frames(session.id());
                pipeline
                    .video_frame_prepare_worker
                    .flush_generation(generation);
            }
            pipeline.dovi_pipeline.reset();
            if !release_first {
                pipeline.output_scheduler.begin_decode_recovery(
                    pipeline.active_recovery_transaction_id(),
                    target_nsecs,
                    DecodeRecoverySource::VulkanReopenReplay,
                    control,
                    session.id(),
                );
                pipeline
                    .video_decode_recovery
                    .reset_for_timeline_start(pipeline.video_stream.codec_id, target_nsecs);
            }
            let Some(ticket) = vo_queue.request_vulkan_prewarm(session.id(), device.clone()) else {
                pipeline
                    .video_decode_pipeline
                    .fail_hevc_same_hardware_recovery(
                        "new Vulkan device prewarm was rejected for the active playback session",
                    );
                return Ok(true);
            };
            pipeline
                .video_decode_pipeline
                .record_hevc_same_hardware_prewarm_request(ticket)?;
            let playback_video_info = playback_video_info_from_worker(
                pipeline.video_stream,
                pipeline.video_decode_pipeline.info(),
            );
            let _ = event_tx.send(BackendEvent::new(
                session.id(),
                BackendEventKind::PlaybackInfoChanged(playback_video_info),
            ));
            tracing::info!(
                session_id = ?session.id(),
                target_nsecs,
                discarded_vo_frames,
                released_scheduler_frames,
                release_first,
                vulkan_device = device.key(),
                same_hw_reopen_attempt = 1,
                same_hw_reopen_result = "opened_waiting_for_renderer_prewarm",
                "atomically replaced the HEVC decoder with a new Vulkan worker"
            );
            Ok(true)
        }
        HevcDecodeRecoveryAction::ReplaySameHardware => Ok(true),
        HevcDecodeRecoveryAction::RebuildFromCachedSeek => {
            let target_nsecs = pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_target()
                .ok_or_else(|| "cached safe-IDR rebuild has no recovery target".to_string())?;
            let transaction_id = pipeline.active_recovery_transaction_id();
            let position_seconds = nsecs_to_seconds(target_nsecs);
            let previous_session_start_nsecs = session.start_position_nsecs();
            let previous_pipeline_start_nsecs = pipeline.current_start_position_nsecs;
            let seek_generation = control.request_seek();
            session.reset_to(session.id(), position_seconds);
            pipeline.current_start_position_nsecs = session.start_position_nsecs();
            let demux_seek_result = service_playback_seek_reset(PlaybackSeekResetContext {
                position_seconds,
                seek_mode: crate::player::backend::PlaybackSeekMode::Precise,
                seek_generation,
                force_low_level_seek: false,
                cache_only: true,
                require_safe_cached_anchor: true,
                preserve_hevc_same_hardware_recovery: true,
                recovery_transaction_id: Some(transaction_id),
                low_level_seek_reason: Some("same_vulkan_reopen_replay_failed"),
                session_id: session.id(),
                vo_queue,
                demux_cache,
                pipeline,
                emit_playback_buffered_events,
                buffering_policy: PlaybackSeekBufferingPolicy::PreserveVisibleFrame,
                control,
                event_tx,
            });
            let demux_seek_result = match demux_seek_result {
                Ok(result) => result,
                Err(error) => {
                    pipeline
                        .video_decode_pipeline
                        .fail_hevc_same_hardware_cached_rebuild(format!(
                            "cached safe-IDR rebuild reset failed: {error}"
                        ));
                    tracing::warn!(
                        session_id = ?session.id(),
                        transaction_id,
                        target_nsecs,
                        seek_generation,
                        %error,
                        "cached safe-IDR rebuild reset failed after Vulkan reopen"
                    );
                    return Ok(true);
                }
            };
            match demux_seek_result {
                DemuxSeekResult::Cached(info) => {
                    debug_assert!(info.anchor_is_safe_seek_point);
                    let generation = pipeline.playback_generation.current();
                    if let Err(error) = pipeline
                        .video_decode_pipeline
                        .begin_hevc_same_hardware_cached_rebuild(generation, Instant::now())
                    {
                        pipeline
                            .video_decode_pipeline
                            .fail_hevc_same_hardware_cached_rebuild(format!(
                                "cached safe-IDR rebuild could not start: {error}"
                            ));
                        return Ok(true);
                    }
                    pipeline.output_scheduler.begin_decode_recovery(
                        transaction_id,
                        target_nsecs,
                        DecodeRecoverySource::CachedSafeIdrRebuild,
                        control,
                        session.id(),
                    );
                    pipeline
                        .output_scheduler
                        .mark_decode_recovery_replaying(transaction_id);
                    tracing::warn!(
                        session_id = ?session.id(),
                        transaction_id,
                        target_nsecs,
                        seek_generation,
                        playback_generation = generation,
                        range_id = info.range_id,
                        anchor_packet_id = info.anchor_packet_id,
                        anchor_kind = info.anchor_kind.as_str(),
                        anchor_nsecs = info.anchor_nsecs,
                        preroll_nsecs = info.preroll_nsecs,
                        "rebuilding reopened Vulkan decoder from cached safe IDR after replay failure"
                    );
                }
                DemuxSeekResult::Unavailable => {
                    session.reset_to(session.id(), nsecs_to_seconds(previous_session_start_nsecs));
                    pipeline.current_start_position_nsecs = previous_pipeline_start_nsecs;
                    pipeline
                        .video_decode_pipeline
                        .fail_hevc_same_hardware_cached_rebuild(
                            "demux cache has no preceding safe IDR covering the recovery target",
                        );
                    tracing::warn!(
                        session_id = ?session.id(),
                        transaction_id,
                        target_nsecs,
                        seek_generation,
                        "cached safe-IDR rebuild was unavailable after Vulkan reopen"
                    );
                }
                DemuxSeekResult::Superseded => {
                    session.reset_to(session.id(), nsecs_to_seconds(previous_session_start_nsecs));
                    pipeline.current_start_position_nsecs = previous_pipeline_start_nsecs;
                    pipeline
                        .video_decode_pipeline
                        .finish_hevc_same_hardware_recovery_terminal();
                    tracing::debug!(
                        session_id = ?session.id(),
                        transaction_id,
                        target_nsecs,
                        seek_generation,
                        "cancelled cached safe-IDR rebuild for a newer seek"
                    );
                }
                DemuxSeekResult::Requested => {
                    pipeline
                        .video_decode_pipeline
                        .fail_hevc_same_hardware_cached_rebuild(
                            "cache-only safe-IDR rebuild unexpectedly requested a low-level seek",
                        );
                }
            }
            Ok(true)
        }
        HevcDecodeRecoveryAction::RequestSoftwareFallback => {
            fallback_to_software_after_same_hardware_recovery(
                session,
                control,
                demux_cache,
                pipeline,
                vo_queue,
                event_tx,
                emit_playback_buffered_events,
            )
        }
        HevcDecodeRecoveryAction::FailExplicitly => {
            let error = pipeline
                .video_decode_pipeline
                .hevc_same_hardware_recovery_terminal_error(now)
                .unwrap_or_else(|| "ForceVulkan 同硬解恢复失败，但缺少终态诊断".to_string());
            pipeline
                .video_decode_pipeline
                .finish_hevc_same_hardware_recovery_terminal();
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fallback_to_software_after_same_hardware_recovery(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    demux_cache: &DemuxPacketCache,
    pipeline: &mut PlaybackPipelineState,
    vo_queue: &VideoOutputQueue,
    event_tx: &Sender<BackendEvent>,
    emit_playback_buffered_events: bool,
) -> std::result::Result<bool, String> {
    let now = Instant::now();
    let target_nsecs = pipeline
        .video_decode_pipeline
        .hevc_same_hardware_recovery_target()
        .ok_or_else(|| "software fallback requested without a same-hardware target".to_string())?;
    let terminal_error = pipeline
        .video_decode_pipeline
        .hevc_same_hardware_recovery_terminal_error(now)
        .unwrap_or_else(|| "bounded same-Vulkan recovery exhausted".to_string());
    if pipeline.video_decode_pipeline.requested_hardware_mode() != super::HardwareDecodeMode::Auto {
        return Err(format!(
            "software fallback invariant violation for {:?}: {terminal_error}",
            pipeline.video_decode_pipeline.requested_hardware_mode()
        ));
    }

    let reopened = pipeline
        .video_decode_pipeline
        .reopen_software_decoder(pipeline.video_stream)?;
    if !reopened {
        return Err(format!(
            "same-Vulkan recovery requested software fallback but decoder was already software: {terminal_error}"
        ));
    }
    let discarded_vo_frames = vo_queue.discard_pending_frames(session.id());
    let generation = pipeline.advance_playback_generation();
    pipeline
        .video_frame_prepare_worker
        .flush_generation(generation);
    pipeline.output_scheduler.begin_decode_recovery(
        pipeline.active_recovery_transaction_id(),
        target_nsecs,
        DecodeRecoverySource::SoftwareFallback,
        control,
        session.id(),
    );
    pipeline
        .video_decode_recovery
        .reset_for_timeline_start(pipeline.video_stream.codec_id, target_nsecs);
    pipeline.dovi_pipeline.reset();
    let playback_video_info = playback_video_info_from_worker(
        pipeline.video_stream,
        pipeline.video_decode_pipeline.info(),
    );
    let _ = event_tx.send(BackendEvent::new(
        session.id(),
        BackendEventKind::PlaybackInfoChanged(playback_video_info),
    ));
    let replay_packets = pipeline
        .video_decode_pipeline
        .requeue_hevc_hw_replay_journal(
            &mut pipeline.playback_generation,
            target_nsecs,
            session.id(),
        )?;
    pipeline
        .video_decode_pipeline
        .finish_hevc_same_hardware_recovery_terminal();
    tracing::warn!(
        session_id = ?session.id(),
        target_nsecs,
        discarded_vo_frames,
        replay_packets,
        terminal_error,
        "Auto mode exhausted same-Vulkan recovery and opened the software decoder"
    );
    if replay_packets > 0 {
        pipeline
            .video_decode_recovery
            .begin_verified_replay_from_safe_anchor(pipeline.video_stream.codec_id, target_nsecs);
        pipeline
            .output_scheduler
            .mark_decode_recovery_replaying(pipeline.active_recovery_transaction_id());
        pipeline.begin_cached_seek_recovery_watchdog(target_nsecs, session.id());
        return Ok(true);
    }

    let position_seconds = nsecs_to_seconds(target_nsecs);
    let seek_generation = control.request_seek();
    session.reset_to(session.id(), position_seconds);
    pipeline.current_start_position_nsecs = session.start_position_nsecs();
    let demux_seek_result = service_playback_seek_reset(PlaybackSeekResetContext {
        position_seconds,
        seek_mode: crate::player::backend::PlaybackSeekMode::Precise,
        seek_generation,
        force_low_level_seek: true,
        cache_only: false,
        require_safe_cached_anchor: false,
        preserve_hevc_same_hardware_recovery: false,
        recovery_transaction_id: Some(pipeline.active_recovery_transaction_id()),
        low_level_seek_reason: Some("same_vulkan_recovery_exhausted"),
        session_id: session.id(),
        vo_queue,
        demux_cache,
        pipeline,
        emit_playback_buffered_events,
        buffering_policy: PlaybackSeekBufferingPolicy::PreserveVisibleFrame,
        control,
        event_tx,
    })?;
    pipeline.output_scheduler.begin_decode_recovery(
        pipeline.active_recovery_transaction_id(),
        target_nsecs,
        DecodeRecoverySource::LowLevelSeek,
        control,
        session.id(),
    );
    pipeline
        .output_scheduler
        .mark_decode_recovery_replaying(pipeline.active_recovery_transaction_id());
    tracing::warn!(
        session_id = ?session.id(),
        target_nsecs,
        seek_generation,
        ?demux_seek_result,
        "software fallback had no replay journal and performed one exact low-level seek"
    );
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn drain_video_decode_results_before_watchdog(
    session_id: PlaybackSessionId,
    control: &FfmpegControl,
    demux_cache: &DemuxPacketCache,
    pipeline: &mut PlaybackPipelineState,
    vo_queue: &VideoOutputQueue,
    event_tx: &Sender<BackendEvent>,
    decode_pipeline: &mut DecodePipelineService,
    frame_presented: &AtomicBool,
) -> std::result::Result<bool, String> {
    let Some(deadline) = pipeline
        .video_decode_pipeline
        .hevc_startup_stall_watchdog_deadline()
    else {
        return Ok(false);
    };
    if Instant::now() < deadline {
        return Ok(false);
    }

    let before = pipeline.video_decode_pipeline.snapshot();
    let max_passes = before
        .command_queue_capacity
        .saturating_add(before.queue_capacity)
        .saturating_add(1)
        .max(1);
    let mut made_progress = false;
    let mut passes = 0usize;
    while passes < max_passes {
        passes = passes.saturating_add(1);
        let status = decode_pipeline.service_once(DecodePipelineServiceContext {
            pipeline,
            control,
            session_id,
            event_tx,
            vo_queue,
            frame_presented,
            demux_reader_watermark: || demux_cache.cached_reader_watermark(),
        })?;
        made_progress |= status.made_progress();
        if status.interrupted() || !status.made_progress() {
            break;
        }
    }
    if made_progress {
        pipeline
            .video_decode_pipeline
            .observe_hevc_decode_pipeline_progress(Instant::now());
    }
    let after = pipeline.video_decode_pipeline.snapshot();
    tracing::debug!(
        session_id = ?session_id,
        passes,
        made_progress,
        submitted_sequence = after.submitted_sequence,
        result_produced_sequence_before = before.result_produced_sequence,
        result_produced_sequence = after.result_produced_sequence,
        result_consumed_sequence_before = before.result_consumed_sequence,
        result_consumed_sequence = after.result_consumed_sequence,
        submitted_not_consumed_packets_before = before.submitted_not_consumed_packets,
        submitted_not_consumed_packets = after.submitted_not_consumed_packets,
        completed_packets = after.completed_packets,
        decoded_frames = after.queued_frames,
        oldest_submitted_packet_nsecs = ?after.oldest_submitted_packet_nsecs,
        last_worker_progress_ms = ?after.last_result_produced_at.map(|at| {
            Instant::now().saturating_duration_since(at).as_secs_f64() * 1000.0
        }),
        "drained FFmpeg video worker results before HEVC startup watchdog decision"
    );
    Ok(made_progress
        || before.result_consumed_sequence != after.result_consumed_sequence
        || before.result_produced_sequence != after.result_produced_sequence)
}

#[allow(clippy::too_many_arguments)]
fn service_hevc_startup_stall_watchdog_due_if_needed(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    demux_cache: &DemuxPacketCache,
    pipeline: &mut PlaybackPipelineState,
    vo_queue: &VideoOutputQueue,
    event_tx: &Sender<BackendEvent>,
    emit_playback_buffered_events: bool,
    playback_wait: &PlaybackPipelineWaitService,
    decode_pipeline: &mut DecodePipelineService,
    frame_presented: &AtomicBool,
    missing_recovery_request_tracker: &mut MissingRecoveryRequestTracker,
    checkpoint: &'static str,
) -> std::result::Result<bool, String> {
    if control.is_user_paused() {
        return Ok(false);
    }
    let drained_progress = drain_video_decode_results_before_watchdog(
        session.id(),
        control,
        demux_cache,
        pipeline,
        vo_queue,
        event_tx,
        decode_pipeline,
        frame_presented,
    )?;
    if drained_progress {
        return Ok(true);
    }
    let Some(tick_status) = service_hevc_startup_stall_watchdog_if_due(
        session.id(),
        pipeline,
        demux_cache.cached_reader_watermark(),
        checkpoint,
    )?
    else {
        return Ok(false);
    };
    if let PlaybackTickStatus::RecoveryPending(request) = tick_status {
        if service_cached_seek_recovery_fallback_if_needed(
            session,
            control,
            demux_cache,
            pipeline,
            vo_queue,
            event_tx,
            emit_playback_buffered_events,
            Some(request),
        )? {
            log_recovery_request_miss_summary(missing_recovery_request_tracker, session.id());
            return Ok(true);
        }
        wait_after_missing_recovery_request(
            pipeline,
            playback_wait,
            missing_recovery_request_tracker,
            request,
            session.id(),
            checkpoint,
        );
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn service_cached_seek_recovery_fallback_if_needed(
    session: &mut PlaybackSession,
    control: &FfmpegControl,
    demux_cache: &DemuxPacketCache,
    pipeline: &mut PlaybackPipelineState,
    vo_queue: &VideoOutputQueue,
    event_tx: &Sender<BackendEvent>,
    emit_playback_buffered_events: bool,
    requested_recovery: Option<PlaybackRecoveryRequest>,
) -> std::result::Result<bool, String> {
    if control.is_user_paused() {
        return Ok(false);
    }
    let (cached_fallback, hevc_fallback, requested_recovery) =
        match take_next_recovery_fallback(pipeline, session.id(), requested_recovery) {
            RecoveryFallbackArbitration::CachedSeek(fallback) => (Some(fallback), None, None),
            RecoveryFallbackArbitration::HevcDecodeChain { request, fallback } => {
                (None, Some(fallback), request)
            }
            RecoveryFallbackArbitration::MissingRequested(_)
            | RecoveryFallbackArbitration::None => {
                return Ok(false);
            }
        };
    if let Some(fallback) = cached_fallback {
        let transaction_id = pipeline.active_recovery_transaction_id();
        let cleared_landing = pipeline
            .video_decode_pipeline
            .clear_hevc_low_level_seek_recovery();
        let position_seconds = nsecs_to_seconds(fallback.target_nsecs);
        control.set_cache_paused(false);
        tracing::debug!(
            session_id = ?session.id(),
            transaction_id,
            recovery_scope = pipeline.video_decode_recovery.recovery_scope().as_str(),
            target_nsecs = fallback.target_nsecs,
            fallback_source = "cached_seek_watchdog",
            fallback_reason = fallback.reason.as_str(),
            actual_anchor_nsecs = ?cleared_landing.map(|landing| landing.anchor_nsecs),
            actual_anchor_kind = ?cleared_landing.map(|landing| landing.anchor_kind.as_str()),
            arbitration_outcome = "fallback_consumed",
            fallback_consumed = true,
            fallback_cleared = cleared_landing.is_some(),
            "consumed atomic cached-seek recovery fallback request"
        );
        if pipeline
            .video_decode_pipeline
            .hevc_same_hardware_recovery_target()
            .is_some()
        {
            let action = pipeline
                .video_decode_pipeline
                .request_hevc_same_hardware_recovery(
                    cached_seek_fallback_as_hevc(fallback),
                    Instant::now(),
                );
            pipeline.video_decode_recovery.reset();
            if action == HevcDecodeRecoveryAction::None {
                pipeline.rearm_cached_seek_recovery_watchdog(
                    fallback.target_nsecs,
                    fallback.cached_seek,
                    session.id(),
                );
            }
            tracing::warn!(
                session_id = ?session.id(),
                position_seconds,
                target_nsecs = fallback.target_nsecs,
                cached_action = fallback.action.as_str(),
                recovery_action = action.as_str(),
                "continued active same-Vulkan transaction before cached-seek escalation"
            );
            return Ok(true);
        }
        match fallback.action {
            CachedSeekRecoveryFallbackAction::RecoveryExhausted => {
                return Err(format!(
                    "HEVC cached seek recovery exhausted at {:.3}s after soft recovery, bounded decoder recovery and low-level seek",
                    position_seconds
                ));
            }
            CachedSeekRecoveryFallbackAction::SoftRecover => {
                let requeued_probe_packets =
                    pipeline.soft_recover_cached_seek_hevc_decode_chain(session.id())?;
                pipeline.rearm_cached_seek_recovery_watchdog(
                    fallback.target_nsecs,
                    fallback.cached_seek,
                    session.id(),
                );
                tracing::debug!(
                    session_id = ?session.id(),
                    position_seconds,
                    target_nsecs = fallback.target_nsecs,
                    reason = fallback.reason.as_str(),
                    requeued_probe_packets,
                    "handled HEVC cached seek recovery fallback with soft decode recovery"
                );
                return Ok(true);
            }
            CachedSeekRecoveryFallbackAction::RecoverHardware => {
                let action = pipeline
                    .video_decode_pipeline
                    .request_hevc_same_hardware_recovery(
                        cached_seek_fallback_as_hevc(fallback),
                        Instant::now(),
                    );
                if action == HevcDecodeRecoveryAction::None
                    && pipeline
                        .video_decode_pipeline
                        .hevc_same_hardware_recovery_target()
                        .is_none()
                {
                    return Err(format!(
                        "cached seek requested hardware recovery at {:.3}s without an active hardware decoder",
                        position_seconds
                    ));
                }
                pipeline.video_decode_recovery.reset();
                if action == HevcDecodeRecoveryAction::None {
                    pipeline.rearm_cached_seek_recovery_watchdog(
                        fallback.target_nsecs,
                        fallback.cached_seek,
                        session.id(),
                    );
                }
                tracing::warn!(
                    session_id = ?session.id(),
                    position_seconds,
                    target_nsecs = fallback.target_nsecs,
                    reason = fallback.reason.as_str(),
                    recovery_action = action.as_str(),
                    "routed cached-seek fallback into bounded same-Vulkan recovery"
                );
                return Ok(true);
            }
            CachedSeekRecoveryFallbackAction::LowLevelSeek => {}
        }
        let demux_watermark = demux_cache.cached_reader_watermark();
        let low_level_seek_required = matches!(
            fallback.action,
            CachedSeekRecoveryFallbackAction::LowLevelSeek
        );
        let failed_cra_cached_seek = fallback
            .cached_seek
            .filter(|info| info.uses_cra_anchor() && low_level_seek_required);
        if let Some(info) = failed_cra_cached_seek {
            demux_cache.exclude_failed_cached_seek_range(info, fallback.reason.as_str());
            tracing::warn!(
                session_id = ?session.id(),
                range_id = info.range_id,
                anchor_packet_id = info.anchor_packet_id,
                anchor_kind = info.anchor_kind.as_str(),
                anchor_nsecs = info.anchor_nsecs,
                target_nsecs = info.target_nsecs,
                preroll_nsecs = info.preroll_nsecs,
                reason = fallback.reason.as_str(),
                cached_seek_succeeded = false,
                low_level_fallback = true,
                "CRA cached seek recovery failed; performing its single low-level fallback"
            );
        }
        if !low_level_seek_required
            && !demux_reader_unusable_for_hevc_low_level_seek(demux_watermark)
        {
            pipeline.rearm_cached_seek_recovery_watchdog(
                fallback.target_nsecs,
                fallback.cached_seek,
                session.id(),
            );
            tracing::debug!(
                session_id = ?session.id(),
                position_seconds,
                target_nsecs = fallback.target_nsecs,
                reason = fallback.reason.as_str(),
                action = fallback.action.as_str(),
                hevc_boundary_reset_required = true,
                reset_path = "forced_low_level",
                demux_video_forward_nsecs = ?demux_watermark.video_forward_nsecs,
                demux_selected_min_forward_nsecs = ?demux_watermark.selected_min_forward_nsecs,
                demux_underrun = demux_watermark.underrun,
                demux_video_underrun = demux_watermark.video_underrun,
                "deferring HEVC cached seek recovery low-level seek while demux reader is still usable"
            );
            return Ok(true);
        }
        let seek_generation = control.request_seek();
        session.reset_to(session.id(), position_seconds);
        pipeline.current_start_position_nsecs = session.start_position_nsecs();
        tracing::debug!(
            session_id = ?session.id(),
            position_seconds,
            target_nsecs = fallback.target_nsecs,
            reason = fallback.reason.as_str(),
            action = fallback.action.as_str(),
            seek_generation,
            hevc_boundary_reset_required = true,
            reset_path = "forced_low_level",
            demux_video_forward_nsecs = ?demux_watermark.video_forward_nsecs,
            demux_selected_min_forward_nsecs = ?demux_watermark.selected_min_forward_nsecs,
            "handling HEVC cached seek recovery fallback with low-level seek"
        );
        let buffering_policy = if failed_cra_cached_seek.is_some() {
            PlaybackSeekBufferingPolicy::PreserveVisibleFrame
        } else {
            internal_recovery_seek_buffering_policy(pipeline.output_scheduler.snapshot())
        };
        let demux_seek_result = service_playback_seek_reset(PlaybackSeekResetContext {
            position_seconds,
            seek_mode: crate::player::backend::PlaybackSeekMode::Precise,
            seek_generation,
            force_low_level_seek: true,
            cache_only: false,
            require_safe_cached_anchor: false,
            preserve_hevc_same_hardware_recovery: false,
            recovery_transaction_id: Some(transaction_id),
            low_level_seek_reason: Some(fallback.reason.as_str()),
            session_id: session.id(),
            vo_queue,
            demux_cache,
            pipeline,
            emit_playback_buffered_events,
            buffering_policy,
            control,
            event_tx,
        })?;
        pipeline.output_scheduler.begin_decode_recovery(
            transaction_id,
            fallback.target_nsecs,
            DecodeRecoverySource::LowLevelSeek,
            control,
            session.id(),
        );
        pipeline
            .output_scheduler
            .mark_decode_recovery_replaying(transaction_id);
        pipeline
            .video_decode_pipeline
            .remember_hevc_recovery_low_level_seek_target(fallback.target_nsecs);
        tracing::debug!(
            session_id = ?session.id(),
            position_seconds,
            target_nsecs = fallback.target_nsecs,
            reason = fallback.reason.as_str(),
            action = fallback.action.as_str(),
            seek_generation,
            hevc_boundary_reset_required = true,
            reset_path = "forced_low_level",
            ?demux_seek_result,
            "handled HEVC cached seek recovery fallback with low-level seek"
        );
        return Ok(true);
    }
    let fallback = hevc_fallback.expect("recovery arbitration selected a HEVC fallback");
    if let Some(request) = requested_recovery {
        let source_matches = matches!(
            request.source,
            PlaybackRecoverySource::HevcDecodeChain(reason) if reason == fallback.reason
        );
        if request.transaction_id != pipeline.active_recovery_transaction_id()
            || request.target_nsecs != fallback.target_nsecs
            || !source_matches
        {
            tracing::error!(
                session_id = ?session.id(),
                requested_transaction_id = request.transaction_id,
                active_transaction_id = pipeline.active_recovery_transaction_id(),
                requested_source = request.source.as_str(),
                requested_target_nsecs = request.target_nsecs,
                fallback_target_nsecs = fallback.target_nsecs,
                fallback_reason = fallback.reason.as_str(),
                arbitration_outcome = "request_mismatch_consumed_safely",
                "HEVC recovery request changed before atomic fallback consumption"
            );
        }
    }
    let transaction_id = pipeline.active_recovery_transaction_id();
    let cleared_landing = pipeline
        .video_decode_pipeline
        .clear_hevc_low_level_seek_recovery();
    let position_seconds = nsecs_to_seconds(fallback.target_nsecs);
    control.set_cache_paused(false);
    tracing::debug!(
        session_id = ?session.id(),
        transaction_id,
        recovery_scope = ?pipeline.video_decode_recovery.recovery_scope().as_str(),
        target_nsecs = fallback.target_nsecs,
        fallback_source = "hevc_decode_chain",
        fallback_reason = fallback.reason.as_str(),
        actual_anchor_nsecs = ?cleared_landing.map(|landing| landing.anchor_nsecs),
        actual_anchor_kind = ?cleared_landing.map(|landing| landing.anchor_kind.as_str()),
        arbitration_outcome = "fallback_consumed",
        fallback_consumed = true,
        fallback_cleared = cleared_landing.is_some(),
        "consumed atomic HEVC recovery fallback request"
    );

    if fallback.reason.invalidated_by_video_progress()
        && pipeline
            .video_decode_pipeline
            .hevc_recent_video_progress_grace_active(Instant::now())
    {
        pipeline.video_decode_recovery.reset();
        pipeline
            .video_decode_pipeline
            .reset_hevc_decode_chain_transient_state();
        tracing::debug!(
            session_id = ?session.id(),
            position_seconds,
            target_nsecs = fallback.target_nsecs,
            reason = fallback.reason.as_str(),
            "discarded stale HEVC decode chain fallback after recent decoded video progress"
        );
        return Ok(true);
    }

    if pipeline.video_decode_pipeline.info().hardware_accelerated
        && hevc_decode_chain_fallback_requests_same_hardware_recovery(fallback.reason)
    {
        let action = pipeline
            .video_decode_pipeline
            .request_hevc_same_hardware_recovery(fallback, Instant::now());
        pipeline.video_decode_recovery.reset();
        if action == HevcDecodeRecoveryAction::None {
            pipeline.begin_cached_seek_recovery_watchdog(fallback.target_nsecs, session.id());
        }
        tracing::warn!(
            session_id = ?session.id(),
            position_seconds,
            target_nsecs = fallback.target_nsecs,
            reason = fallback.reason.as_str(),
            recovery_action = action.as_str(),
            "routed HEVC hardware fallback into bounded same-Vulkan recovery"
        );
        return Ok(true);
    }

    if let Some(info) = pipeline.active_cra_cached_seek() {
        let position_seconds = nsecs_to_seconds(info.target_nsecs);
        pipeline.clear_cached_seek_recovery_watchdog();
        demux_cache.exclude_failed_cached_seek_range(info, fallback.reason.as_str());
        let seek_generation = control.request_seek();
        session.reset_to(session.id(), position_seconds);
        pipeline.current_start_position_nsecs = session.start_position_nsecs();
        tracing::warn!(
            session_id = ?session.id(),
            position_seconds,
            range_id = info.range_id,
            anchor_packet_id = info.anchor_packet_id,
            anchor_kind = info.anchor_kind.as_str(),
            anchor_nsecs = info.anchor_nsecs,
            target_nsecs = info.target_nsecs,
            preroll_nsecs = info.preroll_nsecs,
            reason = fallback.reason.as_str(),
            seek_generation,
            cached_seek_succeeded = false,
            low_level_fallback = true,
            preserve_visible_frame = true,
            "CRA cached seek decode chain failed; performing its single low-level fallback"
        );
        let demux_seek_result = service_playback_seek_reset(PlaybackSeekResetContext {
            position_seconds,
            seek_mode: crate::player::backend::PlaybackSeekMode::Precise,
            seek_generation,
            force_low_level_seek: true,
            cache_only: false,
            require_safe_cached_anchor: false,
            preserve_hevc_same_hardware_recovery: false,
            recovery_transaction_id: Some(transaction_id),
            low_level_seek_reason: Some(fallback.reason.as_str()),
            session_id: session.id(),
            vo_queue,
            demux_cache,
            pipeline,
            emit_playback_buffered_events,
            buffering_policy: PlaybackSeekBufferingPolicy::PreserveVisibleFrame,
            control,
            event_tx,
        })?;
        pipeline
            .video_decode_pipeline
            .remember_hevc_recovery_low_level_seek_target(info.target_nsecs);
        tracing::debug!(
            session_id = ?session.id(),
            range_id = info.range_id,
            anchor_packet_id = info.anchor_packet_id,
            anchor_kind = info.anchor_kind.as_str(),
            target_nsecs = info.target_nsecs,
            seek_generation,
            ?demux_seek_result,
            "completed CRA cached seek decode-error low-level fallback transaction"
        );
        return Ok(true);
    }

    if pipeline.video_decode_pipeline.info().hardware_accelerated
        && fallback.reason.requires_repeat_before_hardware_downgrade()
        && !pipeline
            .video_decode_pipeline
            .has_prior_matching_hevc_decode_chain_fallback(fallback)
    {
        pipeline.video_decode_recovery.reset();
        pipeline
            .video_decode_pipeline
            .reset_hevc_decode_chain_transient_state();
        pipeline
            .video_decode_pipeline
            .remember_hevc_decode_chain_fallback(fallback);
        pipeline.begin_cached_seek_recovery_watchdog(fallback.target_nsecs, session.id());
        tracing::warn!(
            session_id = ?session.id(),
            position_seconds,
            target_nsecs = fallback.target_nsecs,
            reason = fallback.reason.as_str(),
            "deferred HEVC hardware decoder downgrade until recovery failure repeats"
        );
        return Ok(true);
    }

    let loop_action = pipeline
        .video_decode_pipeline
        .hevc_decode_chain_fallback_loop_action(fallback);
    if loop_action == HevcDecodeChainFallbackLoopAction::RecoveryExhausted {
        return Err(format!(
            "HEVC 解码链恢复失败：目标 {:.3}s 在 cached、软件解码和低层 seek 后仍无视频输出（{}）",
            position_seconds,
            fallback.reason.as_str(),
        ));
    }
    if loop_action == HevcDecodeChainFallbackLoopAction::SuppressLowLevelSeek {
        pipeline.video_decode_recovery.reset();
        pipeline
            .video_decode_pipeline
            .reset_hevc_decode_chain_transient_state();
        pipeline
            .video_decode_pipeline
            .remember_hevc_decode_chain_software_suppression(fallback);
        pipeline.begin_cached_seek_recovery_watchdog(fallback.target_nsecs, session.id());
        tracing::warn!(
            session_id = ?session.id(),
            target_nsecs = fallback.target_nsecs,
            reason = fallback.reason.as_str(),
            "suppressing repeated HEVC decode chain fallback low-level seek on software decoder"
        );
        return Ok(true);
    }

    // Hardware decoders have already been routed through the bounded
    // same-Vulkan transaction above. Reaching this point means the pipeline is
    // software-decoded and only its existing low-level seek policy applies.
    let requeued_probe_packets = 0usize;
    let software_reopened_without_replay = false;
    let demux_watermark = demux_cache.cached_reader_watermark();
    let output_snapshot = pipeline.output_scheduler.snapshot();
    let startup_or_post_seek =
        output_snapshot.first_video_frame_pending || output_snapshot.video_bootstrap_after_seek;
    if hevc_decode_chain_fallback_should_suppress_low_level_seek(
        fallback.reason,
        fallback.target_nsecs,
        requeued_probe_packets,
        demux_watermark,
        startup_or_post_seek,
        software_reopened_without_replay,
    ) {
        pipeline.video_decode_recovery.reset();
        pipeline
            .video_decode_pipeline
            .reset_hevc_decode_chain_transient_state();
        pipeline
            .video_decode_pipeline
            .remember_hevc_decode_chain_fallback(fallback);
        tracing::warn!(
            session_id = ?session.id(),
            reason = fallback.reason.as_str(),
            target_ms = fallback.target_nsecs as f64 / 1_000_000.0,
            probe_packets = requeued_probe_packets,
            demux_forward_ms = ?demux_watermark
                .video_forward_nsecs
                .or(demux_watermark.selected_min_forward_nsecs)
                .map(|duration| duration as f64 / 1_000_000.0),
            startup_or_post_seek,
            queued_video_ms = output_snapshot.queued_video_duration_nsecs as f64 / 1_000_000.0,
            "hevc_low_level_seek_suppressed"
        );
        return Ok(true);
    }
    let boundary_reset_required =
        hevc_decode_chain_fallback_requires_boundary_reset(fallback.reason);
    let force_low_level_from_loop =
        loop_action == HevcDecodeChainFallbackLoopAction::ForceLowLevelSeek;
    if !software_reopened_without_replay
        && !force_low_level_from_loop
        && !boundary_reset_required
        && !demux_reader_unusable_for_hevc_low_level_seek(demux_watermark)
    {
        pipeline
            .video_decode_pipeline
            .remember_hevc_decode_chain_fallback(fallback);
        tracing::debug!(
            session_id = ?session.id(),
            position_seconds,
            target_nsecs = fallback.target_nsecs,
            reason = fallback.reason.as_str(),
            hevc_boundary_reset_required = boundary_reset_required,
            reset_path = "forced_low_level",
            demux_video_forward_nsecs = ?demux_watermark.video_forward_nsecs,
            demux_selected_min_forward_nsecs = ?demux_watermark.selected_min_forward_nsecs,
            demux_underrun = demux_watermark.underrun,
            demux_video_underrun = demux_watermark.video_underrun,
            "deferring HEVC decode chain low-level seek while demux reader is still usable"
        );
        return Ok(true);
    }
    let seek_generation = control.request_seek();
    session.reset_to(session.id(), position_seconds);
    pipeline.current_start_position_nsecs = session.start_position_nsecs();
    let force_low_level_seek = force_low_level_from_loop || !boundary_reset_required;
    let reset_path = if force_low_level_seek {
        "forced_low_level"
    } else if boundary_reset_required {
        "cached_then_low_level"
    } else {
        "forced_low_level"
    };
    tracing::debug!(
        session_id = ?session.id(),
        position_seconds,
        target_nsecs = fallback.target_nsecs,
        reason = fallback.reason.as_str(),
        seek_generation,
        hevc_boundary_reset_required = boundary_reset_required,
        reset_path,
        demux_video_forward_nsecs = ?demux_watermark.video_forward_nsecs,
        demux_selected_min_forward_nsecs = ?demux_watermark.selected_min_forward_nsecs,
        demux_underrun = demux_watermark.underrun,
        demux_video_underrun = demux_watermark.video_underrun,
        "handling HEVC decode chain recovery fallback with boundary reset"
    );
    let demux_seek_result = service_playback_seek_reset(PlaybackSeekResetContext {
        position_seconds,
        seek_mode: crate::player::backend::PlaybackSeekMode::Precise,
        seek_generation,
        force_low_level_seek,
        cache_only: false,
        require_safe_cached_anchor: false,
        preserve_hevc_same_hardware_recovery: false,
        recovery_transaction_id: Some(transaction_id),
        low_level_seek_reason: Some(fallback.reason.as_str()),
        session_id: session.id(),
        vo_queue,
        demux_cache,
        pipeline,
        emit_playback_buffered_events,
        buffering_policy: internal_recovery_seek_buffering_policy(output_snapshot),
        control,
        event_tx,
    })?;
    pipeline.output_scheduler.begin_decode_recovery(
        transaction_id,
        fallback.target_nsecs,
        DecodeRecoverySource::LowLevelSeek,
        control,
        session.id(),
    );
    pipeline
        .output_scheduler
        .mark_decode_recovery_replaying(transaction_id);
    if force_low_level_seek {
        pipeline
            .video_decode_pipeline
            .remember_hevc_decode_chain_low_level_seek(fallback);
    } else {
        pipeline
            .video_decode_pipeline
            .remember_hevc_decode_chain_fallback(fallback);
    }
    tracing::debug!(
        session_id = ?session.id(),
        position_seconds,
        target_nsecs = fallback.target_nsecs,
        reason = fallback.reason.as_str(),
        seek_generation,
        hevc_boundary_reset_required = boundary_reset_required,
        reset_path,
        ?demux_seek_result,
        "handled HEVC decode chain recovery fallback with boundary reset"
    );
    Ok(true)
}

fn hevc_decode_chain_fallback_requests_same_hardware_recovery(
    reason: HevcDecodeChainFallbackReason,
) -> bool {
    matches!(
        reason,
        HevcDecodeChainFallbackReason::ZeroOutputRebuffer
            | HevcDecodeChainFallbackReason::StartupInFlightStall
            | HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput
            | HevcDecodeChainFallbackReason::RecoveryWaitRebuffer
            | HevcDecodeChainFallbackReason::PostFallbackRebufferUnderfill
    )
}

fn cached_seek_fallback_as_hevc(fallback: CachedSeekRecoveryFallback) -> HevcDecodeChainFallback {
    HevcDecodeChainFallback {
        target_nsecs: fallback.target_nsecs,
        reason: match fallback.reason {
            CachedSeekRecoveryFallbackReason::FirstVideoFrameTimeout => {
                HevcDecodeChainFallbackReason::StartupInFlightStall
            }
            CachedSeekRecoveryFallbackReason::VideoPacketLimit => {
                HevcDecodeChainFallbackReason::ZeroOutputRebuffer
            }
        },
    }
}

fn hevc_decode_chain_fallback_requires_boundary_reset(
    reason: HevcDecodeChainFallbackReason,
) -> bool {
    reason.requires_boundary_reset()
}

fn demux_reader_unusable_for_hevc_low_level_seek(watermark: DemuxReaderWatermark) -> bool {
    let video_forward_empty = watermark.video_forward_nsecs.unwrap_or_default() == 0;
    let selected_forward_empty = watermark.selected_min_forward_nsecs.unwrap_or_default() == 0;
    watermark.video_underrun && video_forward_empty && selected_forward_empty
}

fn demux_reader_healthy_for_hevc_low_level_seek_suppression(
    watermark: DemuxReaderWatermark,
) -> bool {
    let video_forward_nsecs = watermark
        .video_forward_nsecs
        .or(watermark.selected_min_forward_nsecs)
        .unwrap_or_default();
    !watermark.video_underrun
        && video_forward_nsecs >= duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
}

fn hevc_decode_chain_fallback_should_suppress_low_level_seek(
    reason: HevcDecodeChainFallbackReason,
    target_nsecs: u64,
    probe_packets: usize,
    demux_watermark: DemuxReaderWatermark,
    startup_or_post_seek: bool,
    software_reopened_without_replay: bool,
) -> bool {
    !software_reopened_without_replay
        && matches!(
            reason,
            HevcDecodeChainFallbackReason::ZeroOutputRebuffer
                | HevcDecodeChainFallbackReason::RecoveryWaitRebuffer
        )
        && target_nsecs == 0
        && probe_packets == 0
        && startup_or_post_seek
        && demux_reader_healthy_for_hevc_low_level_seek_suppression(demux_watermark)
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

#[cfg(test)]
mod tests {
    use super::{
        AudioRealignCoverage, AudioRealignExecutionDecision, DemuxReaderWatermark,
        HevcDecodeChainFallback, HevcDecodeChainFallbackReason, MissingRecoveryRequestTracker,
        PlaybackOutputSnapshot, PlaybackRecoveryRequest, PlaybackRecoverySource,
        PlaybackSeekBufferingPolicy, RecoveryFallbackArbitration, RecoveryFallbackSource,
        audio_realign_execution_decision, cached_input_admission,
        demux_reader_unusable_for_hevc_low_level_seek,
        hevc_decode_chain_fallback_requests_same_hardware_recovery,
        hevc_decode_chain_fallback_requires_boundary_reset,
        hevc_decode_chain_fallback_should_suppress_low_level_seek,
        internal_recovery_seek_buffering_policy, rebuffer_audio_realign_can_preserve_video_queue,
        rebuffer_audio_realign_requires_low_level_seek, take_next_recovery_fallback,
    };
    use crate::player::backend::ffmpeg::playback_loop::PlaybackOutputState;
    use crate::player::render_host::PlaybackSessionId;

    struct UnreadyCraRecoverySource {
        cra_closed_range_ready: bool,
        cached_fallback_polls: usize,
        hevc_fallback_takes: usize,
        pending_hevc_fallback: Option<HevcDecodeChainFallback>,
    }

    impl RecoveryFallbackSource for UnreadyCraRecoverySource {
        type CachedFallback = ();
        type HevcFallback = HevcDecodeChainFallback;

        fn take_cached_seek_fallback(
            &mut self,
            _session_id: PlaybackSessionId,
        ) -> Option<Self::CachedFallback> {
            self.cached_fallback_polls += 1;
            self.cra_closed_range_ready.then_some(())
        }

        fn take_hevc_decode_chain_fallback(&mut self) -> Option<Self::HevcFallback> {
            self.hevc_fallback_takes += 1;
            self.pending_hevc_fallback.take()
        }
    }

    fn output_snapshot(
        state: PlaybackOutputState,
        queued_video_frames: usize,
        rebuffering: bool,
        video_output_low_water: bool,
        video_decode_underfill: bool,
    ) -> PlaybackOutputSnapshot {
        PlaybackOutputSnapshot {
            state,
            first_video_frame_pending: state.first_video_frame_pending(),
            first_frame_needed: state.first_video_frame_pending(),
            first_frame_presented: !state.first_video_frame_pending(),
            initial_av_start_pending: state.first_video_frame_pending(),
            output_clock_running: state == PlaybackOutputState::Playing,
            audio_start_target_nsecs: None,
            output_transition_deadline_ms: None,
            rebuffering,
            queued_video_frames,
            recovery_staging_frames: 0,
            recovery_staging_frame_budget: None,
            committed_output_high_water_nsecs: Some(1_800_000_000),
            recovery_staged_high_water_nsecs: None,
            decode_recovery_audio_ready_latched: false,
            queued_video_coverage_nsecs: 800_000_000,
            queued_video_duration_nsecs: 800_000_000,
            queued_video_range_span_nsecs: 800_000_000,
            queued_video_range_nsecs: Some((1_000_000_000, 1_800_000_000)),
            queued_video_forward_nsecs: Some(800_000_000),
            queued_video_contiguous_forward_nsecs: Some(800_000_000),
            queued_video_largest_gap_nsecs: None,
            video_output_low_water,
            pending_start_audio_frames: 0,
            pending_start_audio_nsecs: 0,
            video_output_rebuffer_anchor: None,
            video_bootstrap_after_seek: false,
            video_decode_underfill,
            rebuffer_empty_audio_output_blocked: false,
            scheduler_dropped_video_frames: 0,
            recent_coordinator_stall_nsecs: None,
            recent_coordinator_stall_age_nsecs: None,
        }
    }

    #[test]
    fn primed_cached_input_remains_admissible_until_output_lead_is_throttled() {
        let mut output = output_snapshot(PlaybackOutputState::Syncing, 38, false, false, false);
        output.first_frame_needed = false;
        output.initial_av_start_pending = true;
        output.audio_start_target_nsecs = Some(184_714_739_000);

        let transaction_blocked = cached_input_admission(true, false, output);
        assert!(transaction_blocked.input_admissible);
        assert!(!transaction_blocked.output_transaction_blocked);

        output.first_frame_needed = true;
        let first_frame_rearmed = cached_input_admission(true, false, output);
        assert!(first_frame_rearmed.input_admissible);
        assert!(!first_frame_rearmed.output_transaction_blocked);

        let lead_throttled = cached_input_admission(true, true, output);
        assert!(!lead_throttled.input_admissible);
        assert!(!lead_throttled.output_transaction_blocked);
    }

    #[test]
    fn missing_recovery_request_logs_once_and_aggregates_repeated_ticks() {
        let request = PlaybackRecoveryRequest {
            transaction_id: 91,
            source: PlaybackRecoverySource::HevcDecodeChain(
                HevcDecodeChainFallbackReason::RecoveryWaitRebuffer,
            ),
            target_nsecs: 237_237_000_000,
        };
        let mut tracker = MissingRecoveryRequestTracker::default();

        assert!(tracker.record(request));
        for _ in 0..10_000 {
            assert!(!tracker.record(request));
        }
        assert_eq!(tracker.take_summary(), Some((request, 10_001)));
        assert!(tracker.record(request));
    }

    #[test]
    fn unclosed_cra_range_cannot_starve_requested_hevc_fallback() {
        let fallback = HevcDecodeChainFallback {
            target_nsecs: 235_235_000_000,
            reason: HevcDecodeChainFallbackReason::RecoveryWaitRebuffer,
        };
        let request = PlaybackRecoveryRequest {
            transaction_id: 73,
            source: PlaybackRecoverySource::HevcDecodeChain(fallback.reason),
            target_nsecs: fallback.target_nsecs,
        };
        let mut source = UnreadyCraRecoverySource {
            cra_closed_range_ready: false,
            cached_fallback_polls: 0,
            hevc_fallback_takes: 0,
            pending_hevc_fallback: Some(fallback),
        };

        assert_eq!(
            take_next_recovery_fallback(&mut source, PlaybackSessionId(9), Some(request)),
            RecoveryFallbackArbitration::HevcDecodeChain {
                request: Some(request),
                fallback,
            }
        );
        assert_eq!(source.cached_fallback_polls, 0);
        assert_eq!(source.hevc_fallback_takes, 1);
        assert_eq!(source.pending_hevc_fallback, None);
    }

    #[test]
    fn queued_audio_realign_is_cancelled_after_live_coverage_reaches_waterline() {
        let target_nsecs = 237_237_000_000;
        let pending_coverage = AudioRealignCoverage {
            audio_accepted_start_timeline_nsecs: Some(target_nsecs),
            start_gap_nsecs: Some(0),
            contiguous_coverage_nsecs: Some(938_999_996),
            protected_target_nsecs: 850_000_000,
            ready: true,
        };

        assert_eq!(
            audio_realign_execution_decision(target_nsecs, pending_coverage, None, 0).0,
            AudioRealignExecutionDecision::CoverageSatisfied
        );
    }

    #[test]
    fn queued_audio_realign_waits_while_decoder_input_can_fill_gap() {
        let target_nsecs = 237_237_000_000;
        let missing_coverage = AudioRealignCoverage {
            protected_target_nsecs: 850_000_000,
            ..AudioRealignCoverage::default()
        };

        assert_eq!(
            audio_realign_execution_decision(target_nsecs, missing_coverage, None, 1).0,
            AudioRealignExecutionDecision::InputPending
        );
    }

    #[test]
    fn queued_audio_realign_is_cancelled_when_audio_output_covers_target() {
        let target_nsecs = 237_237_000_000;
        let missing_pending_coverage = AudioRealignCoverage {
            protected_target_nsecs: 850_000_000,
            ..AudioRealignCoverage::default()
        };

        let (decision, output_coverage_nsecs) = audio_realign_execution_decision(
            target_nsecs,
            missing_pending_coverage,
            Some((target_nsecs, target_nsecs + 938_999_996)),
            0,
        );

        assert_eq!(decision, AudioRealignExecutionDecision::CoverageSatisfied);
        assert_eq!(output_coverage_nsecs, Some(938_999_996));
    }

    #[test]
    fn hevc_startup_zero_output_requests_same_hardware_recovery() {
        assert!(hevc_decode_chain_fallback_requests_same_hardware_recovery(
            HevcDecodeChainFallbackReason::ZeroOutputRebuffer
        ));
    }

    #[test]
    fn hevc_startup_in_flight_stall_requests_same_hardware_recovery() {
        assert!(hevc_decode_chain_fallback_requests_same_hardware_recovery(
            HevcDecodeChainFallbackReason::StartupInFlightStall
        ));
    }

    #[test]
    fn hevc_recovery_wait_rebuffer_requests_same_hardware_recovery() {
        assert!(hevc_decode_chain_fallback_requests_same_hardware_recovery(
            HevcDecodeChainFallbackReason::RecoveryWaitRebuffer
        ));
    }

    #[test]
    fn hevc_pts_gap_requests_same_hardware_recovery_before_seek() {
        assert!(hevc_decode_chain_fallback_requests_same_hardware_recovery(
            HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput
        ));
    }

    #[test]
    fn internal_recovery_suppresses_buffering_while_visible_output_is_healthy() {
        assert_eq!(
            internal_recovery_seek_buffering_policy(output_snapshot(
                PlaybackOutputState::Playing,
                48,
                false,
                false,
                false,
            )),
            PlaybackSeekBufferingPolicy::PreserveVisibleFrame
        );
        assert_eq!(
            internal_recovery_seek_buffering_policy(output_snapshot(
                PlaybackOutputState::Playing,
                3,
                false,
                true,
                false,
            )),
            PlaybackSeekBufferingPolicy::Emit
        );
        assert_eq!(
            internal_recovery_seek_buffering_policy(output_snapshot(
                PlaybackOutputState::Rebuffering,
                0,
                true,
                true,
                true,
            )),
            PlaybackSeekBufferingPolicy::Emit
        );
    }

    #[test]
    fn repeated_rebuffer_audio_realign_never_implies_low_level_seek() {
        assert!(!rebuffer_audio_realign_requires_low_level_seek(2, true));
        assert!(!rebuffer_audio_realign_requires_low_level_seek(2, false));
        assert!(!rebuffer_audio_realign_requires_low_level_seek(1, false));
    }

    #[test]
    fn rebuffer_audio_realign_service_preserves_only_first_covering_audio_realign() {
        assert!(!rebuffer_audio_realign_can_preserve_video_queue(
            2, true, true
        ));
        assert!(!rebuffer_audio_realign_requires_low_level_seek(2, true));
        assert!(rebuffer_audio_realign_can_preserve_video_queue(
            1, true, true
        ));
        assert!(!rebuffer_audio_realign_can_preserve_video_queue(
            2, false, true
        ));
        assert!(!rebuffer_audio_realign_can_preserve_video_queue(
            2, true, false
        ));
    }

    #[test]
    fn successful_cra_video_coverage_survives_first_audio_only_repair() {
        let attempts = 1;
        let cra_cached_video_queue_covers_target = true;
        let audio_pipeline_available = true;

        assert!(rebuffer_audio_realign_can_preserve_video_queue(
            attempts,
            cra_cached_video_queue_covers_target,
            audio_pipeline_available,
        ));
        assert!(!rebuffer_audio_realign_requires_low_level_seek(
            attempts,
            cra_cached_video_queue_covers_target,
        ));
    }

    #[test]
    fn hevc_decode_chain_hard_fallbacks_require_boundary_reset() {
        for reason in [
            HevcDecodeChainFallbackReason::ZeroOutputRebuffer,
            HevcDecodeChainFallbackReason::StartupInFlightStall,
            HevcDecodeChainFallbackReason::RecoveryWaitRebuffer,
            HevcDecodeChainFallbackReason::PostFallbackRebufferUnderfill,
            HevcDecodeChainFallbackReason::PtsGapAfterZeroOutput,
        ] {
            assert!(hevc_decode_chain_fallback_requires_boundary_reset(reason));
        }
    }

    #[test]
    fn hevc_decode_chain_boundary_reset_bypasses_forward_cache_deferral() {
        let demux_watermark = DemuxReaderWatermark {
            video_forward_nsecs: Some(1_000_000_000),
            selected_min_forward_nsecs: Some(1_000_000_000),
            video_underrun: false,
            underrun: false,
            ..DemuxReaderWatermark::default()
        };

        assert!(hevc_decode_chain_fallback_requires_boundary_reset(
            HevcDecodeChainFallbackReason::ZeroOutputRebuffer
        ));
        assert!(!demux_reader_unusable_for_hevc_low_level_seek(
            demux_watermark
        ));
    }

    #[test]
    fn hevc_low_level_seek_waits_while_demux_reader_has_video_forward_cache() {
        assert!(!demux_reader_unusable_for_hevc_low_level_seek(
            DemuxReaderWatermark {
                video_forward_nsecs: Some(1_000_000_000),
                selected_min_forward_nsecs: Some(1_000_000_000),
                video_underrun: false,
                underrun: false,
                ..DemuxReaderWatermark::default()
            }
        ));
    }

    #[test]
    fn hevc_low_level_seek_requires_video_reader_underrun() {
        assert!(demux_reader_unusable_for_hevc_low_level_seek(
            DemuxReaderWatermark {
                video_forward_nsecs: Some(0),
                selected_min_forward_nsecs: Some(0),
                video_underrun: true,
                underrun: true,
                ..DemuxReaderWatermark::default()
            }
        ));
    }

    #[test]
    fn hevc_low_level_seek_ignores_audio_only_underrun_with_video_forward_cache() {
        assert!(!demux_reader_unusable_for_hevc_low_level_seek(
            DemuxReaderWatermark {
                video_forward_nsecs: Some(2_000_000_000),
                audio_forward_nsecs: Some(0),
                selected_min_forward_nsecs: Some(0),
                audio_underrun: true,
                underrun: true,
                ..DemuxReaderWatermark::default()
            }
        ));
    }

    #[test]
    fn hevc_recovery_wait_zero_target_suppresses_low_level_seek_when_demux_is_healthy() {
        assert!(hevc_decode_chain_fallback_should_suppress_low_level_seek(
            HevcDecodeChainFallbackReason::RecoveryWaitRebuffer,
            0,
            0,
            DemuxReaderWatermark {
                video_forward_nsecs: Some(2_000_000_000),
                selected_min_forward_nsecs: Some(2_000_000_000),
                video_underrun: false,
                underrun: false,
                ..DemuxReaderWatermark::default()
            },
            true,
            false,
        ));
    }

    #[test]
    fn hevc_recovery_wait_zero_target_does_not_suppress_after_probe_requeue() {
        assert!(!hevc_decode_chain_fallback_should_suppress_low_level_seek(
            HevcDecodeChainFallbackReason::RecoveryWaitRebuffer,
            0,
            1,
            DemuxReaderWatermark {
                video_forward_nsecs: Some(2_000_000_000),
                selected_min_forward_nsecs: Some(2_000_000_000),
                video_underrun: false,
                underrun: false,
                ..DemuxReaderWatermark::default()
            },
            true,
            false,
        ));
    }

    #[test]
    fn empty_safe_replay_after_software_reopen_cannot_skip_seek_fallback() {
        assert!(!hevc_decode_chain_fallback_should_suppress_low_level_seek(
            HevcDecodeChainFallbackReason::RecoveryWaitRebuffer,
            0,
            0,
            DemuxReaderWatermark {
                video_forward_nsecs: Some(2_000_000_000),
                selected_min_forward_nsecs: Some(2_000_000_000),
                ..DemuxReaderWatermark::default()
            },
            true,
            true,
        ));
    }
}
