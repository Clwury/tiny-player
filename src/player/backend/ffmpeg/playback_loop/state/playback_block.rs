use super::{
    AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION, AUDIO_VIDEO_QUEUE_TARGET_DURATION,
    VULKAN_VIDEO_OUTPUT_RESOURCE_PRESSURE_FRAMES, duration_nsecs,
};

const VULKAN_HW_SURFACE_RESERVE_FRAMES: usize = 6;
const VULKAN_IN_FLIGHT_FRAME_MARGIN: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) struct VideoOutputResourcePressure {
    pub(in crate::player::backend::ffmpeg) scheduled_video_frames: usize,
    pub(in crate::player::backend::ffmpeg) decoded_video_frames: usize,
    pub(in crate::player::backend::ffmpeg) submitted_not_consumed_video_packets: usize,
    pub(in crate::player::backend::ffmpeg) prepare_worker_frames: usize,
    pub(in crate::player::backend::ffmpeg) recovery_staging_frames: usize,
    pub(in crate::player::backend::ffmpeg) hardware_accelerated: bool,
    pub(in crate::player::backend::ffmpeg) scheduled_video_queue_limit_reached: bool,
    pub(in crate::player::backend::ffmpeg) decode_recovery_video_admission_blocked: bool,
    pub(in crate::player::backend::ffmpeg) fill_phase_for_output_start: bool,
    pub(in crate::player::backend::ffmpeg) video_frame_duration_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) vo_queue_capacity: usize,
    pub(in crate::player::backend::ffmpeg) vo_queued_frames: usize,
    pub(in crate::player::backend::ffmpeg) queued_video_forward_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) audio_output_pending_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg) render_backlogged: bool,
}

impl VideoOutputResourcePressure {
    pub(in crate::player::backend::ffmpeg) fn active_frames(self) -> usize {
        self.scheduled_video_frames
            .saturating_add(self.decoded_video_frames)
            .saturating_add(self.submitted_not_consumed_video_packets)
            .saturating_add(self.prepare_worker_frames)
            .saturating_add(self.recovery_staging_frames)
            .saturating_add(self.vo_queued_frames)
    }

    pub(in crate::player::backend::ffmpeg) fn dynamic_frame_budget(self) -> usize {
        let target_frames = frames_for_duration(
            self.video_frame_duration_nsecs,
            duration_nsecs(AUDIO_VIDEO_QUEUE_TARGET_DURATION),
        );
        target_frames
            .saturating_add(self.vo_queue_capacity.max(self.vo_queued_frames))
            .saturating_add(VULKAN_HW_SURFACE_RESERVE_FRAMES)
            .saturating_add(VULKAN_IN_FLIGHT_FRAME_MARGIN)
            .max(VULKAN_VIDEO_OUTPUT_RESOURCE_PRESSURE_FRAMES)
    }

    pub(in crate::player::backend::ffmpeg) fn hard_frame_budget(self) -> usize {
        self.dynamic_frame_budget()
            .saturating_add(VULKAN_IN_FLIGHT_FRAME_MARGIN)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum PlaybackBlockReason {
    DemuxCache,
    PacketQueueFull,
    DecoderRecovery,
    DecoderInFlight,
    DecoderOutputPending,
    DecoderInputEmpty,
    DecodedVideoQueue,
    DecodedQueueFull,
    HwSurfacePool,
    FramePrepareWorker,
    DecodedAudioQueue,
    AudioOutput,
    VideoOutputQueue,
    RenderWorker,
    OutputGate,
}

impl PlaybackBlockReason {
    pub(in crate::player::backend::ffmpeg) fn as_str(self) -> &'static str {
        match self {
            Self::DemuxCache => "demux_cache",
            Self::PacketQueueFull => "packet_queue_full",
            Self::DecoderRecovery => "decoder_recovery",
            Self::DecoderInFlight => "decoder_in_flight",
            Self::DecoderOutputPending => "decoder_output_pending",
            Self::DecoderInputEmpty => "decoder_input_empty",
            Self::DecodedVideoQueue => "decoded_video_queue",
            Self::DecodedQueueFull => "decoded_queue_full",
            Self::HwSurfacePool => "hw_surface_pool",
            Self::FramePrepareWorker => "frame_prepare_worker",
            Self::DecodedAudioQueue => "decoded_audio_queue",
            Self::AudioOutput => "audio_output",
            Self::VideoOutputQueue => "vo_queue",
            Self::RenderWorker => "render_worker",
            Self::OutputGate => "output_gate",
        }
    }
}

fn frames_for_duration(frame_duration_nsecs: u64, target_nsecs: u64) -> usize {
    if frame_duration_nsecs == 0 || target_nsecs == 0 {
        return 0;
    }
    let frames =
        target_nsecs.saturating_add(frame_duration_nsecs.saturating_sub(1)) / frame_duration_nsecs;
    usize::try_from(frames).unwrap_or(usize::MAX)
}

pub(in crate::player::backend::ffmpeg) fn video_output_resource_pressure(
    pressure: VideoOutputResourcePressure,
) -> bool {
    if pressure.scheduled_video_queue_limit_reached
        || pressure.decode_recovery_video_admission_blocked
    {
        return true;
    }
    if !pressure.hardware_accelerated {
        return false;
    }

    // The absolute ownership budget includes every place that can retain a
    // Vulkan AVFrame. It applies even while startup/rebuffer fill is active;
    // only the lower steady-state pressure threshold is relaxed below.
    if pressure.active_frames() >= pressure.hard_frame_budget() {
        return true;
    }

    // During the rebuffer / first-frame fill phase the soft Vulkan frame-pressure
    // threshold must NOT throttle decode. The rebuffer resume waterline can require
    // more decoded frames than this soft threshold permits, so throttling here would
    // deadlock against the resume: decode stops -> the decoded window never reaches
    // the resume target -> playback never resumes -> the queued frames are never
    // presented -> the frame-pool pressure never clears. Only the hard scheduled-queue
    // limit (checked above) applies while filling; steady-state playback keeps the
    // soft threshold below.
    if pressure.fill_phase_for_output_start {
        return false;
    }

    // mpv keeps the VO side demand-driven: transient low output/audio watermarks
    // should request more decode work instead of allowing a soft Vulkan budget to
    // masquerade as a full decoded queue. Real hw surface exhaustion is still
    // reported by the decoder as HwSurfacePool.
    if pressure
        .queued_video_forward_nsecs
        .is_none_or(|forward| forward < duration_nsecs(AUDIO_VIDEO_QUEUE_TARGET_DURATION))
        || pressure
            .audio_output_pending_nsecs
            .is_some_and(|pending| pending < duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION))
    {
        return false;
    }

    if pressure.render_backlogged {
        return true;
    }

    pressure.active_frames() >= pressure.dynamic_frame_budget()
}

pub(in crate::player::backend::ffmpeg) fn video_decode_block_reason_with_output_queue(
    decoder_blocked_on: Option<PlaybackBlockReason>,
    output_resource_pressure: bool,
) -> Option<PlaybackBlockReason> {
    if output_resource_pressure {
        Some(PlaybackBlockReason::DecodedVideoQueue)
    } else {
        decoder_blocked_on
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION, AUDIO_VIDEO_QUEUE_TARGET_DURATION,
        PlaybackBlockReason, VideoOutputResourcePressure, duration_nsecs,
        video_decode_block_reason_with_output_queue, video_output_resource_pressure,
    };

    fn pressure_for_test(
        scheduled_video_frames: usize,
        decoded_video_frames: usize,
        submitted_not_consumed_video_packets: usize,
    ) -> VideoOutputResourcePressure {
        VideoOutputResourcePressure {
            scheduled_video_frames,
            decoded_video_frames,
            submitted_not_consumed_video_packets,
            prepare_worker_frames: 0,
            recovery_staging_frames: 0,
            hardware_accelerated: true,
            scheduled_video_queue_limit_reached: false,
            decode_recovery_video_admission_blocked: false,
            fill_phase_for_output_start: false,
            video_frame_duration_nsecs: 20_000_000,
            vo_queue_capacity: 3,
            vo_queued_frames: 0,
            queued_video_forward_nsecs: Some(duration_nsecs(AUDIO_VIDEO_QUEUE_TARGET_DURATION)),
            audio_output_pending_nsecs: Some(duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION)),
            render_backlogged: false,
        }
    }

    #[test]
    fn video_decode_block_reason_prefers_scheduled_video_queue_pressure() {
        assert_eq!(
            video_decode_block_reason_with_output_queue(
                Some(PlaybackBlockReason::DecoderInputEmpty),
                true,
            ),
            Some(PlaybackBlockReason::DecodedVideoQueue)
        );
        assert_eq!(
            video_decode_block_reason_with_output_queue(None, true),
            Some(PlaybackBlockReason::DecodedVideoQueue)
        );
        assert_eq!(
            video_decode_block_reason_with_output_queue(
                Some(PlaybackBlockReason::PacketQueueFull),
                false,
            ),
            Some(PlaybackBlockReason::PacketQueueFull)
        );
    }

    #[test]
    fn video_output_resource_pressure_uses_hardware_shared_frame_budget() {
        let pressure = pressure_for_test(39, 3, 1);
        assert_eq!(pressure.dynamic_frame_budget(), 43);
        assert!(video_output_resource_pressure(pressure));
        assert!(!video_output_resource_pressure(pressure_for_test(38, 3, 1)));

        let mut software = pressure_for_test(60, 0, 0);
        software.hardware_accelerated = false;
        assert!(!video_output_resource_pressure(software));

        let mut hard_limit = software;
        hard_limit.scheduled_video_queue_limit_reached = true;
        assert!(video_output_resource_pressure(hard_limit));
    }

    #[test]
    fn vulkan_ownership_budget_counts_every_external_frame_holder() {
        let mut pressure = pressure_for_test(1, 2, 3);
        pressure.prepare_worker_frames = 4;
        pressure.recovery_staging_frames = 5;
        pressure.vo_queued_frames = 6;

        assert_eq!(pressure.active_frames(), 1 + 2 + 3 + 4 + 5 + 6);
    }

    #[test]
    fn video_output_resource_pressure_relaxes_soft_threshold_during_output_fill() {
        // Fill phase (rebuffer/first-frame): the soft Vulkan threshold is ignored so
        // decode can reach the resume waterline instead of deadlocking against it...
        let mut pressure = pressure_for_test(46, 0, 0);
        pressure.fill_phase_for_output_start = true;
        assert!(!video_output_resource_pressure(pressure));
        // ...but the all-owner hard frame budget still applies while filling.
        pressure.scheduled_video_frames = 47;
        assert!(video_output_resource_pressure(pressure));
        pressure.scheduled_video_frames = 46;
        // The explicit recovery staging/input gate is also hard.
        pressure.decode_recovery_video_admission_blocked = true;
        assert!(video_output_resource_pressure(pressure));
        pressure.decode_recovery_video_admission_blocked = false;
        // The hard scheduled-queue limit remains unconditional.
        pressure.scheduled_video_queue_limit_reached = true;
        assert!(video_output_resource_pressure(pressure));
    }

    #[test]
    fn video_output_resource_pressure_relaxes_soft_threshold_on_low_output_watermarks() {
        let mut pressure = pressure_for_test(45, 0, 0);
        pressure.queued_video_forward_nsecs =
            Some(duration_nsecs(AUDIO_VIDEO_QUEUE_TARGET_DURATION) - 1);
        assert!(!video_output_resource_pressure(pressure));

        pressure.queued_video_forward_nsecs =
            Some(duration_nsecs(AUDIO_VIDEO_QUEUE_TARGET_DURATION));
        pressure.audio_output_pending_nsecs =
            Some(duration_nsecs(AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION) - 1);
        assert!(!video_output_resource_pressure(pressure));
    }

    #[test]
    fn video_output_resource_pressure_treats_render_backlog_as_pressure() {
        let mut pressure = pressure_for_test(1, 0, 0);
        pressure.render_backlogged = true;
        assert!(video_output_resource_pressure(pressure));
    }
}
