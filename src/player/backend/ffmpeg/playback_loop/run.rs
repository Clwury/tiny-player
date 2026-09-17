#[path = "run/audio_realign.rs"]
mod audio_realign;
#[path = "run/cached_seek.rs"]
mod cached_seek;
#[path = "run/hevc_recovery.rs"]
mod hevc_recovery;

use audio_realign::{
    internal_recovery_seek_buffering_policy, service_audio_realign_recovery_watchdog_if_needed,
    service_rebuffer_audio_realign_seek_if_needed,
};
use cached_seek::{
    service_cached_seek_recovery_fallback_if_needed,
    service_hevc_startup_stall_watchdog_due_if_needed,
};
use hevc_recovery::{
    drain_video_decode_results_before_watchdog, fallback_to_software_after_same_hardware_recovery,
    service_hevc_same_hardware_recovery_if_needed,
};

#[cfg(test)]
use audio_realign::{
    audio_realign_execution_decision, rebuffer_audio_realign_can_preserve_video_queue,
    rebuffer_audio_realign_requires_low_level_seek,
};
#[cfg(test)]
use cached_seek::{
    demux_reader_unusable_for_hevc_low_level_seek,
    hevc_decode_chain_fallback_requests_same_hardware_recovery,
    hevc_decode_chain_fallback_requires_boundary_reset,
    hevc_decode_chain_fallback_should_suppress_low_level_seek,
};

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
    HevcDecodeRecoveryAction, hevc_decoder_drain_work_pending, hevc_drain_video_result_progressed,
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

/// Shared view of playback resources used by recovery services.
///
/// Keeping these references together makes it possible to split recovery
/// services by concern without changing ownership or lifetime semantics of the
/// playback loop.
struct PlaybackRecoveryContext<'a> {
    session: &'a mut PlaybackSession,
    control: &'a FfmpegControl,
    demux_cache: &'a DemuxPacketCache,
    pipeline: &'a mut PlaybackPipelineState,
    vo_queue: &'a VideoOutputQueue,
    event_tx: &'a Sender<BackendEvent>,
    emit_playback_buffered_events: bool,
}

impl<'a> PlaybackRecoveryContext<'a> {
    fn new(
        session: &'a mut PlaybackSession,
        control: &'a FfmpegControl,
        demux_cache: &'a DemuxPacketCache,
        pipeline: &'a mut PlaybackPipelineState,
        vo_queue: &'a VideoOutputQueue,
        event_tx: &'a Sender<BackendEvent>,
        emit_playback_buffered_events: bool,
    ) -> Self {
        Self {
            session,
            control,
            demux_cache,
            pipeline,
            vo_queue,
            event_tx,
            emit_playback_buffered_events,
        }
    }
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
    control.set_session_id(source.session_id);
    let OpenedPlaybackInput {
        mut input,
        stream_catalog,
        video_stream,
        video_decoder,
        audio_stream,
        audio_decoder: opened_audio_decoder,
        subtitle_stream,
        subtitle_decoder,
    } = open_playback_input_with_fallback(&mut source, Arc::clone(&control), &event_tx)?;
    let mut session = PlaybackSession::new(source.session_id, source.start_position_seconds);
    let initial_playback_file_info = input.playback_file_info();
    let mut video_decode_pipeline = VideoDecodePipeline::spawn(video_decoder)?;
    video_decode_pipeline.set_decoder_framedrop(source.cache_config.decoder_framedrop);
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
    let mut scheduler = PlaybackScheduler::new(current_start_position_nsecs);
    scheduler.set_playback_rate(control.playback_rate());
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

            let mut recovery_context = PlaybackRecoveryContext::new(
                &mut session,
                &control,
                &demux_cache,
                &mut pipeline,
                &video_output_queue,
                &event_tx,
                emit_playback_buffered_events,
            );
            if service_audio_realign_recovery_watchdog_if_needed(&mut recovery_context)? {
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
            let recovering = pipeline
                .output_scheduler
                .playback_output_state
                .restart_pending()
                || pipeline
                    .output_scheduler
                    .playback_output_state
                    .rebuffering()
                || demux_reader_watermark.underrun;
            let recovery_input_required = recovering
                && demux_packet_snapshot.streams.iter().any(|stream| {
                    cache_pause_work
                        .requested_streams
                        .contains(&stream.stream_index)
                        && !stream.consumer_drainable
                });
            // Decoded output pressure controls decoder admission. Compressed
            // prefetch follows its own byte/time budget, also during seek
            // recovery, so a full decoder queue cannot throttle cache refill.
            if let Some(http_cache) = http_cache.as_ref() {
                http_cache.set_recovery_input_required(recovery_input_required);
                http_cache.update_demux_high_water_prefetch_paused(
                    demux_packet_snapshot.total_bytes,
                    demux_packet_snapshot.prefetch_limit_bytes,
                    demux_packet_snapshot.prefetch_queue_full(),
                    demux_reader_watermark.underrun || recovery_input_required,
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
                            !control.is_paused(),
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
