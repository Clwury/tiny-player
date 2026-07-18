use super::{
    AUDIO_CLOCK_VIDEO_PRESENT_LEAD, AUDIO_DECODE_QUEUE_LIMIT_DURATION, AUDIO_OUTPUT_DELAY_LIMIT,
    AUDIO_OUTPUT_QUEUE_LIMIT_DURATION, AUDIO_OUTPUT_STEADY_TARGET_DURATION,
    AUDIO_OUTPUT_UNDERRUN_CLOCK_RESUME_DURATION, AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION,
    AUDIO_OUTPUT_VIDEO_LEAD_DURATION, AUDIO_REBUFFER_DELAYED_START_MAX,
    AUDIO_REBUFFER_LOOP_DETECTION_WINDOW, AUDIO_REBUFFER_PREFILL_LOOP_TARGET,
    AUDIO_REBUFFER_PREFILL_TARGET, AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN,
    AUDIO_VIDEO_QUEUE_LIMIT_DURATION, AUDIO_VIDEO_QUEUE_TARGET_DURATION,
    AUDIO_VIDEO_REBUFFER_DRIFT_RESET_THRESHOLD, AudioClockMode, AudioOutput,
    AudioOutputDrainStatus, AudioOutputPushResult, AudioOutputSnapshot, AudioResampler, AvFrame,
    AvPacket, AvPacketReadDiagnostic, AvPacketStorageKind, BufferedReporter,
    DECODE_PACKET_SLOW_LOG_AFTER, DECODE_PIPELINE_INTERNAL_STAGE_TIMING_LOG_AFTER,
    DEFAULT_VIDEO_FRAME_DURATION_NSECS, DEMUX_CACHE_LOCK_TIMING_LOG_AFTER,
    DEMUX_PACKET_CACHE_LOCK_WAIT, DEMUX_PACKET_CACHE_PREFETCH_PAUSE_LOG_AFTER,
    DEMUX_PACKET_CACHE_PREFETCH_PAUSE_LOG_INTERVAL, DEMUX_PACKET_CACHE_STALL_LOG_AFTER,
    DEMUX_PACKET_CACHE_STALL_LOG_INTERVAL, DEMUX_PACKET_CACHE_WAIT_INTERVAL,
    DEMUX_PUMP_TIMING_LOG_INTERVAL, DEMUX_READ_WAIT_LOG_AFTER, DecodedAudio, DecodedSubtitleCue,
    Decoder, DoviPipeline, FFMPEG_FRAME_COUNT, FfmpegCommand, FfmpegControl, FfmpegPlaybackInput,
    FormatContext, HardwareDecodeMode, HttpRingCache, InputProbeProfile, LATE_VIDEO_DROP_TOLERANCE,
    OUTPUT_GATE_INTERNAL_STAGE_TIMING_LOG_AFTER, PENDING_AUDIO_CONTINUITY_TOLERANCE,
    PENDING_START_AUDIO_BACKPRESSURE_DURATION, PLAYBACK_COORDINATOR_STAGE_TIMING_LOG_AFTER,
    PLAYBACK_COORDINATOR_TICK_TIMING_LOG_AFTER, PLAYING_PENDING_AUDIO_FORCE_RECOVERY_DURATION,
    PLAYING_PENDING_AUDIO_HARD_RESET_DURATION, PgsFrameMergeBitstreamFilter, PlaybackScheduler,
    PositionReporter, QueuedVideoFrame, SCHEDULER_POLL_INTERVAL, StreamInfo, TimestampMapper,
    VIDEO_DECODE_SKIP_NONREF_LOW_WATER_DURATION, VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER,
    VIDEO_OUTPUT_REBUFFER_ENTER_AFTER, VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION,
    VIDEO_OUTPUT_REBUFFER_MIN_STABLE_RESUME_DURATION, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
    VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER, VIDEO_OUTPUT_START_AV_SYNC_TOLERANCE,
    VIDEO_OUTPUT_START_FAST_READY_DURATION, VIDEO_OUTPUT_START_FIRST_FRAME_FALLBACK_AFTER,
    VIDEO_OUTPUT_START_FIRST_FRAME_STALL_LOG_AFTER, VIDEO_OUTPUT_START_PREBUFFER_DURATION,
    VIDEO_OUTPUT_STARTUP_DEMUX_FALLBACK_AFTER, VIDEO_OUTPUT_UNDERRUN_FAST_RECOVERY_AFTER,
    VULKAN_DECODED_VIDEO_QUEUE_LIMIT_FRAMES, VULKAN_VIDEO_OUTPUT_RESOURCE_PRESSURE_FRAMES,
    VideoFrameConvertContext, VideoFrameConverter, VideoRecoveryPointKind,
    WORKER_CHANNEL_RECV_WAIT_LOG_AFTER, WORKER_CHANNEL_SEND_WAIT_LOG_AFTER,
    align_audio_elements_to_frame_boundary, audio_codec_requires_recovery_point,
    audio_elements_for_duration_floor, audio_elements_for_frames, audio_frames_for_duration_round,
    coalesce_playback_seek_commands, decoded_subrip_packet_cue, drain_playback_commands,
    duration_nsecs, ffmpeg_error, frame_best_effort_timestamp, frame_decode_error_flags,
    frame_is_corrupt, load_external_subtitle_cues, nsecs_to_seconds,
    optional_buffered_value_changed, packet_is_audio_recovery_point,
    packet_is_video_recovery_point, packet_is_video_seek_point, packet_video_recovery_point_kind,
    queued_video_duration, queued_video_frames_have_vulkan, queued_video_limit_duration,
    queued_video_target_duration, seconds_to_nsecs, timestamp_to_nsecs,
};
#[cfg(test)]
use super::{
    DEMUX_PACKET_CACHE_MEMORY_BYTES, VIDEO_OUTPUT_REBUFFER_RESUME_FRAMES,
    VIDEO_OUTPUT_START_PREBUFFER_FRAMES, queued_video_target_frames,
};

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

mod input;
mod media_info;
mod run;
mod seek;
mod subtitle_timeline;

use super::avio::should_cache_http_url;
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
use coordinator_tick::{
    PlaybackTickContext, PlaybackTickStatus, service_hevc_startup_stall_watchdog_if_due,
    service_playback_tick,
};
use decode::PlaybackGeneration;
#[cfg(test)]
pub(super) use decoded_video_frame::{
    DecodedVideoFrameStartAction, decoded_video_frame_start_action,
};
pub(super) use demux_cache::DemuxReaderWatermark;
use demux_cache::{DemuxCachedSeekInfo, DemuxPacketCache, DemuxPacketCacheInput, DemuxReadResult};
#[cfg(test)]
pub(super) use input::initial_probe_profile;
use input::{
    OpenedPlaybackInput, StreamCatalog, load_external_subtitle_cue_list, open_audio_decoder,
    open_playback_input_with_fallback, open_subtitle_decoder,
    select_audio_stream_for_selection_from_catalog,
    select_subtitle_stream_for_selection_from_catalog,
};
use media_info::{playback_audio_info_from_stream, playback_video_info_from_worker};
pub(super) use output_gate::{PlaybackOutputScheduler, PlaybackOutputSnapshot};
#[cfg(test)]
pub(super) use output_rebuffer::{
    AudioClockResumeDecision, InitialOutputSyncDecision, RebufferResumeAnchor, ResumeAnchorSource,
    audio_clock_resume_decision, audio_clock_resume_timeline_nsecs,
    audio_output_buffered_until_for_resume, decoded_audio_forward_nsecs_from,
    decoded_video_start_prebuffer_reached, demux_reader_ready_for_output,
    initial_audio_clock_resume_decision, initial_output_sync_decision,
    initial_playback_resume_waterline, playback_resume_waterline,
    playback_resume_waterline_blocked_on, rebuffer_audio_clock_resume_decision,
    rebuffer_playback_resume_waterline, rebuffer_playback_resume_waterline_after_prolonged_wait,
    rebuffer_playback_resume_waterline_for_decision,
    rebuffer_playback_resume_waterline_with_resource_pressure, should_block_for_demux_read,
    video_decode_should_skip_nonref_for_pressure, video_output_rebuffer_resume_duration,
    video_output_rebuffer_resume_duration_with_resource_pressure,
    video_output_rebuffer_resume_reached, video_output_rebuffer_should_enter,
};
pub(super) use output_rebuffer::{AudioResumeWaterline, PlaybackOutputState};
#[cfg(test)]
pub(super) use pending_audio_queue::PendingStartAudio;
pub(super) use playback_block::PlaybackBlockReason;
use playback_pipeline_state::PlaybackPipelineState;
use playback_services::PlaybackPipelineServices;
use run::playback_buffered_near_duration;
#[cfg(test)]
pub(super) use run::playback_read_finished;
pub(super) use run::run_ffmpeg_playback;
#[cfg(test)]
pub(super) use scheduled_video_queue::{
    audio_clocked_video_wait_duration, discard_queued_video_before, pop_audio_clocked_video_frame,
    pop_audio_clocked_video_frame_with_policy, push_queued_video_frame,
    queued_video_buffered_until_nsecs, queued_video_frame_ready_for_audio_clock,
    should_drop_late_video_frame, video_output_rebuffer_low_water,
};
use seek::{
    preroll_seek_position_seconds, video_cached_seek_preroll_nsecs, video_seek_preroll_nsecs,
};
use session::PlaybackSession;
use subtitle_timeline::{
    packet_duration_nsecs, push_subtitle_cue, refresh_playback_timeline_origin,
    subtitle_cue_queue_from_external, update_subtitle_overlay,
};
#[cfg(test)]
pub(super) use subtitle_timeline::{
    rebase_subtitle_cues_to_timeline_origin, subtitle_timestamp_to_timeline_nsecs,
};
pub(super) use subtitle_timeline::{
    subtitle_cue_timeline_nsecs, trim_overlapping_subtitle_cues_at,
};
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
// Cached HEVC seeks follow mpv's HR cache behavior: seek the demux/cache reader
// before the display target, then let precise decode discard frames up to it.
pub(super) const HEVC_CACHED_SEEK_PREROLL_NSECS: u64 = 500_000_000;
