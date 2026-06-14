use super::VULKAN_VIDEO_OUTPUT_RESOURCE_PRESSURE_FRAMES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg) enum PlaybackBlockReason {
    DemuxCache,
    PacketQueueFull,
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

pub(in crate::player::backend::ffmpeg) fn video_output_resource_pressure(
    scheduled_video_frames: usize,
    decoded_video_frames: usize,
    in_flight_video_packets: usize,
    hardware_accelerated: bool,
    scheduled_video_queue_limit_reached: bool,
    fill_phase_for_output_start: bool,
) -> bool {
    if scheduled_video_queue_limit_reached {
        return true;
    }
    if !hardware_accelerated {
        return false;
    }

    // During the rebuffer / first-frame fill phase the soft Vulkan frame-pressure
    // threshold must NOT throttle decode. The rebuffer resume waterline can require
    // more decoded frames than this soft threshold permits, so throttling here would
    // deadlock against the resume: decode stops -> the decoded window never reaches
    // the resume target -> playback never resumes -> the queued frames are never
    // presented -> the frame-pool pressure never clears. Only the hard scheduled-queue
    // limit (checked above) applies while filling; steady-state playback keeps the
    // soft threshold below.
    if fill_phase_for_output_start {
        return false;
    }

    scheduled_video_frames
        .saturating_add(decoded_video_frames)
        .saturating_add(in_flight_video_packets)
        >= VULKAN_VIDEO_OUTPUT_RESOURCE_PRESSURE_FRAMES
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
    use super::*;

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
        assert!(video_output_resource_pressure(19, 0, 1, true, false, false));
        assert!(video_output_resource_pressure(
            23, 12, 1, true, false, false
        ));
        assert!(!video_output_resource_pressure(
            18, 0, 1, true, false, false
        ));
        assert!(!video_output_resource_pressure(
            19, 0, 1, false, false, false
        ));
        assert!(video_output_resource_pressure(0, 0, 0, false, true, false));
    }

    #[test]
    fn video_output_resource_pressure_relaxes_soft_threshold_during_output_fill() {
        // Fill phase (rebuffer/first-frame): the soft Vulkan threshold is ignored so
        // decode can reach the resume waterline instead of deadlocking against it...
        assert!(!video_output_resource_pressure(
            23, 12, 1, true, false, true
        ));
        assert!(!video_output_resource_pressure(47, 0, 0, true, false, true));
        // ...but the hard scheduled-queue limit still applies even while filling.
        assert!(video_output_resource_pressure(0, 0, 0, true, true, true));
    }
}
