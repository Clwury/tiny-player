use super::*;

pub(super) fn flush_playback_decode_state(
    video_decoder: &Decoder,
    audio_decoder: Option<&Decoder>,
    subtitle_pipeline: &mut SubtitlePipeline,
    video_frame: &mut AvFrame,
    audio_frame: Option<&mut AvFrame>,
    packet: &mut AvPacket,
) {
    video_decoder.flush_buffers();
    if let Some(decoder) = audio_decoder {
        decoder.flush_buffers();
    }
    subtitle_pipeline.flush_decode_state();
    video_frame.unref();
    if let Some(frame) = audio_frame {
        frame.unref();
    }
    packet.unref();
}

pub(super) fn should_drop_backlogged_vulkan_frame(
    frame: *const ffi::AVFrame,
    first_video_frame_pending: bool,
    frame_slot: &FrameSlot,
) -> bool {
    if first_video_frame_pending || !is_vulkan_frame(frame) {
        return false;
    }
    let key_frame = unsafe { (*frame).flags & ffi::AV_FRAME_FLAG_KEY != 0 };
    if key_frame {
        return false;
    }
    let backpressure = frame_slot.render_backpressure();
    if backpressure.should_drop_non_key_frame() {
        tracing::debug!(
            last_render_ms = backpressure.last_render_nsecs / 1_000_000,
            average_render_ms = backpressure.average_render_nsecs / 1_000_000,
            pending = backpressure.pending_requests,
            "dropping Vulkan decoded non-key frame before retaining hardware frame"
        );
        return true;
    }
    false
}
