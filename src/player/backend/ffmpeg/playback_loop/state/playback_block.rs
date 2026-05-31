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
