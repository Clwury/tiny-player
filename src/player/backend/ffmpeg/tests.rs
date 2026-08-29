#[path = "tests/audio.rs"]
mod audio;
#[path = "tests/backend_cache.rs"]
mod backend_cache;
#[path = "tests/control.rs"]
mod control;
#[path = "tests/decode_recovery.rs"]
mod decode_recovery;
#[path = "tests/http_cache.rs"]
mod http_cache;
#[path = "tests/misc.rs"]
mod misc;
#[path = "tests/output.rs"]
mod output;
#[path = "tests/timeline.rs"]
mod timeline;

use super::audio::{AudioShared, fill_audio_output};
use super::avio::{CacheRestartRequest, CachedInputSource, HttpCacheRangeKind};
use super::worker::{FfmpegCommand, PendingSeek, PendingTrackSelection, drain_playback_commands};
use super::{
    AUDIO_OUTPUT_DELAY_LIMIT, AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION,
    AUDIO_OUTPUT_VIDEO_LEAD_DURATION, AUDIO_RESUME_INPUT_SUPPRESSION_MARGIN,
    AUDIO_VIDEO_QUEUE_LIMIT_DURATION, AUDIO_VIDEO_QUEUE_TARGET_DURATION, AudioOutputDecision,
    AudioOutputLifecycle, AudioOutputSnapshot, AvFrame, AvPacket, BackendEvent, BackendEventKind,
    BufferedReporter, ByteCacheState, CacheReadResult, DECODED_VIDEO_QUEUE_LIMIT_FRAMES,
    DEFAULT_VIDEO_FRAME_DURATION_NSECS, DecodedAudio, FALLBACK_AUDIO_OUTPUT_CHANNELS,
    FfmpegBackend, FfmpegControl, FfmpegPlaybackInput, HTTP_CACHE_CHUNK_SIZE,
    HTTP_CACHE_PARTIAL_READ_MIN_BYTES, HTTP_CACHE_RANGE_REQUEST_BYTES,
    HTTP_CACHE_RANGE_REQUEST_TIMEOUT, HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES,
    HTTP_CACHE_SMALL_RANGE_REQUEST_TIMEOUT, HTTP_RING_CACHE_CAPACITY, HttpContentRange,
    HttpRingCache, HttpRingCacheState, InputProbeProfile, MappedTimestamp,
    PENDING_AUDIO_CONTINUITY_TOLERANCE, PENDING_START_AUDIO_BACKPRESSURE_DURATION,
    PGS_SUBTITLE_VIDEO_QUEUE_LIMIT_DURATION, PGS_SUBTITLE_VIDEO_QUEUE_TARGET_DURATION,
    PgsFrameMergeBitstreamFilter, PlaybackCacheByteRange, PlaybackCacheConfig, PlaybackCacheMode,
    PlaybackCacheState, PlaybackScheduler, PlaybackSeekMode, PositionReporter, QueuedVideoFrame,
    StreamInfo, TimestampMapper, VIDEO_OUTPUT_REBUFFER_AUDIO_STALL_FALLBACK_AFTER,
    VIDEO_OUTPUT_REBUFFER_ENTER_AFTER, VIDEO_OUTPUT_REBUFFER_LOW_WATER_DURATION,
    VIDEO_OUTPUT_REBUFFER_RESUME_DURATION, VIDEO_OUTPUT_REBUFFER_STALLED_FALLBACK_AFTER,
    VULKAN_AUDIO_VIDEO_QUEUE_LIMIT_DURATION, VULKAN_AUDIO_VIDEO_QUEUE_TARGET_DURATION,
    VULKAN_DECODED_VIDEO_QUEUE_LIMIT_FRAMES, VULKAN_DECODED_VIDEO_QUEUE_TARGET_FRAMES,
    VULKAN_VIDEO_OUTPUT_RESOURCE_PRESSURE_FRAMES, WaitStatus, audio_sample_len,
    content_len_from_content_range, content_range_from_headers, dovi_packet_timeline_nsecs,
    duration_nsecs, ffmpeg_http_headers, ffmpeg_raw_video_format, frame_decode_error_flags,
    frame_is_corrupt, has_annex_b_start_code, http_cache_playback_range_request_bytes,
    http_cache_range_header, http_cache_range_request_len, http_cache_range_request_timeout,
    http_cache_request_headers_for_log, http_cache_response_headers_for_log,
    optional_buffered_value_changed, queued_video_coverage_duration, queued_video_duration,
    queued_video_limit_duration, queued_video_limit_frames, queued_video_limit_reached,
    queued_video_range_span, queued_video_target_duration, queued_video_target_frames,
    queued_video_target_reached, reqwest_header_pairs, should_cache_http_url,
};
use std::{
    collections::VecDeque,
    mem,
    os::raw::c_int,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

use super::playback_loop::{
    AudioClockResumeDecision, DecodedVideoFrameStartAction, DemuxReaderWatermark,
    InitialOutputSyncDecision, PendingAudioUnderrunRecoveryPlan, PendingStartAudio,
    PlaybackBlockReason, PlaybackOutputScheduler, PlaybackOutputState, RebufferResumeAnchor,
    ResumeAnchorSource, VIDEO_DECODE_RECOVERY_MAX_SKIPPED_PACKETS, VideoDecodeRecovery,
    admit_decoded_video_frame_to_vo, audio_clock_resume_decision,
    audio_clock_resume_timeline_nsecs, audio_clocked_video_wait_duration,
    audio_output_buffered_until_for_resume, decoded_audio_forward_nsecs_from,
    decoded_video_frame_start_action, decoded_video_start_prebuffer_reached,
    demux_reader_ready_for_output, discard_queued_video_before,
    discard_stale_pending_audio_before_recovery_start, initial_audio_clock_resume_decision,
    initial_output_sync_decision, initial_playback_resume_waterline, initial_probe_profile,
    pending_audio_underrun_recovery_plan, playback_read_finished, playback_resume_waterline,
    playback_resume_waterline_blocked_on, pop_audio_clocked_video_frame,
    pop_audio_clocked_video_frame_with_policy, push_queued_video_frame,
    queued_video_buffered_until_nsecs, queued_video_frame_ready_for_audio_clock,
    rebase_subtitle_cues_to_timeline_origin, rebuffer_audio_clock_resume_decision,
    rebuffer_playback_resume_waterline, rebuffer_playback_resume_waterline_after_prolonged_wait,
    rebuffer_playback_resume_waterline_for_decision,
    rebuffer_playback_resume_waterline_with_resource_pressure, should_block_for_demux_read,
    should_drop_late_video_frame, subtitle_cue_timeline_nsecs,
    subtitle_timestamp_to_timeline_nsecs, trim_overlapping_subtitle_cues_at,
    video_decode_error_is_recoverable, video_decode_should_skip_nonref_for_pressure,
    video_output_rebuffer_low_water, video_output_rebuffer_resume_duration,
    video_output_rebuffer_resume_duration_with_resource_pressure,
    video_output_rebuffer_resume_reached, video_output_rebuffer_should_enter,
};
use crate::player::{
    backend::{BackendSubtitleCue, DemuxCacheState, PlaybackCacheTimeRange},
    render_host::{
        DecodedFrame, FfmpegAvBufferRef, FfmpegFrameRef, FrameColor, FramePixels, FramePts,
        PlaybackSessionId, RawVideoChromaSite, RawVideoFormat, RawVideoRange, RenderSize,
        VideoOutputQueue, VulkanDecodeDevice, VulkanDecodeQueue, VulkanDecodeQueues,
        VulkanVideoFrame,
    },
};
use ffmpeg_sys_next as ffi;

fn playback_input_with_selection(
    selected_tracks: crate::player::PlaybackTrackSelection,
) -> FfmpegPlaybackInput {
    FfmpegPlaybackInput {
        session_id: PlaybackSessionId::default(),
        url: "file:///tmp/video.mkv".to_string(),
        http_headers: Vec::new(),
        content_length: None,
        start_position_seconds: 0.0,
        selected_tracks,
        cache_config: PlaybackCacheConfig::default(),
    }
}

fn assert_buffered_event(
    rx: &Receiver<BackendEvent>,
    expected_session_id: PlaybackSessionId,
    expected: Option<f64>,
) {
    match rx.try_recv().expect("expected buffered event") {
        BackendEvent {
            session_id,
            kind: BackendEventKind::BufferedChanged(buffered_until),
        } => {
            assert_eq!(session_id, expected_session_id);
            assert_eq!(buffered_until, expected);
        }
        event => panic!("expected buffered event, got {event:?}"),
    }
}

fn test_packet_from_data(data: &[u8]) -> AvPacket {
    let props = AvPacket::new().expect("packet props allocate");
    AvPacket::from_data_and_props(data, &props).expect("packet data allocates")
}

fn test_queued_video_frames_with_duration(
    start_timeline_nsecs: u64,
    frame_count: usize,
    frame_duration_nsecs: u64,
) -> VecDeque<QueuedVideoFrame> {
    let mut queued = VecDeque::new();
    for index in 0..frame_count {
        let mut frame = test_queued_video_frame(
            start_timeline_nsecs + u64::try_from(index).unwrap() * frame_duration_nsecs,
        );
        frame.duration_nsecs = frame_duration_nsecs;
        queued.push_back(frame);
    }
    queued
}

fn test_vulkan_queued_video_frames_with_duration(
    start_timeline_nsecs: u64,
    frame_count: usize,
    frame_duration_nsecs: u64,
) -> VecDeque<QueuedVideoFrame> {
    let mut queued = VecDeque::new();
    for index in 0..frame_count {
        let mut frame = test_vulkan_queued_video_frame(
            start_timeline_nsecs + u64::try_from(index).unwrap() * frame_duration_nsecs,
        );
        frame.duration_nsecs = frame_duration_nsecs;
        queued.push_back(frame);
    }
    queued
}

fn test_pending_audio(start_timeline_nsecs: u64, duration_nsecs: u64) -> PendingStartAudio {
    let mut pending = PendingStartAudio::default();
    pending.push(
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs,
        },
        start_timeline_nsecs,
        start_timeline_nsecs + duration_nsecs,
    );
    pending
}

fn test_audio_snapshot(
    played_timeline_nsecs: u64,
    total_pending_nsecs: u64,
) -> AudioOutputSnapshot {
    AudioOutputSnapshot {
        played_timeline_nsecs,
        buffered_until_timeline_nsecs: played_timeline_nsecs.saturating_add(total_pending_nsecs),
        shared_pending_nsecs: total_pending_nsecs,
        queue_pending_nsecs: 0,
        total_pending_nsecs,
        queue_frames: 0,
        queue_generation: 0,
        ..AudioOutputSnapshot::default()
    }
}

fn ready_demux_watermark(forward_nsecs: u64) -> DemuxReaderWatermark {
    DemuxReaderWatermark {
        video_forward_nsecs: Some(forward_nsecs),
        audio_forward_nsecs: Some(forward_nsecs),
        selected_min_forward_nsecs: Some(forward_nsecs),
        video_underrun: false,
        audio_underrun: false,
        video_idle: false,
        audio_idle: false,
        underrun: false,
        idle: false,
        forward_bytes: 1024,
    }
}

fn test_queued_video_frame(timeline_nsecs: u64) -> QueuedVideoFrame {
    QueuedVideoFrame {
        frame: DecodedFrame {
            size: RenderSize {
                width: 1,
                height: 1,
            },
            pts: Some(FramePts {
                nsecs: timeline_nsecs,
            }),
            key_frame: false,
            pixels: FramePixels::Bgra8(vec![0, 0, 0, 255].into()),
        },
        timeline_nsecs,
        duration_nsecs: DEFAULT_VIDEO_FRAME_DURATION_NSECS,
        source_duration_nsecs: DEFAULT_VIDEO_FRAME_DURATION_NSECS,
    }
}

fn test_vulkan_queued_video_frame(timeline_nsecs: u64) -> QueuedVideoFrame {
    let mut queued = test_queued_video_frame(timeline_nsecs);
    let mut av_frame = AvFrame::new().expect("FFmpeg frame allocates");
    unsafe {
        (*av_frame.as_mut_ptr()).format = ffi::AVPixelFormat::AV_PIX_FMT_BGRA as c_int;
        (*av_frame.as_mut_ptr()).width = 1;
        (*av_frame.as_mut_ptr()).height = 1;
    }
    let buffer_result = unsafe { ffi::av_frame_get_buffer(av_frame.as_mut_ptr(), 1) };
    assert!(buffer_result >= 0, "FFmpeg frame buffer allocates");

    queued.frame.pixels = FramePixels::VulkanVideo(VulkanVideoFrame {
        frame: FfmpegFrameRef::new_ref(av_frame.as_mut_ptr()).expect("FFmpeg frame refs"),
        device: test_vulkan_device(),
        format: RawVideoFormat::P010Le,
        usage: 0,
        color: FrameColor::Sdr,
        range: RawVideoRange::Limited,
        chroma_site: RawVideoChromaSite::Left,
        metadata: None,
        planes: Vec::new(),
    });
    queued
}

fn test_vulkan_device() -> Arc<VulkanDecodeDevice> {
    let mut buffer = unsafe { ffi::av_buffer_alloc(1) };
    assert!(!buffer.is_null(), "FFmpeg buffer allocates");
    let device_ref = FfmpegAvBufferRef::new_ref(buffer).expect("FFmpeg buffer refs");
    unsafe { ffi::av_buffer_unref(&mut buffer) };
    Arc::new(VulkanDecodeDevice::new(
        device_ref,
        0,
        0,
        0,
        1,
        0,
        0,
        0,
        VulkanDecodeQueues {
            graphics: VulkanDecodeQueue { index: 0, count: 1 },
            compute: None,
            transfer: None,
        },
    ))
}
