use super::{
    audio_decode_pipeline::AudioDecodePipeline, audio_decode_worker::AudioDecodePacketResult,
    decode::PlaybackGeneration, video_decode_pipeline::VideoDecodePipeline,
    video_decode_worker::VideoDecodeDrainResult,
};

pub(super) struct PlaybackDrainPhase {
    video_generation: u64,
    audio_generation: Option<u64>,
    video_result: Option<VideoDecodeDrainResult>,
    audio_result: Option<AudioDecodePacketResult>,
}

pub(super) struct PlaybackDrainResults {
    pub(super) video: VideoDecodeDrainResult,
    pub(super) audio: Option<AudioDecodePacketResult>,
}

impl PlaybackDrainPhase {
    pub(super) fn start(
        playback_generation: &mut PlaybackGeneration,
        video_decode_pipeline: &mut VideoDecodePipeline,
        audio_decode_pipeline: Option<&mut AudioDecodePipeline>,
    ) -> std::result::Result<Self, String> {
        let video_generation = playback_generation.advance();
        video_decode_pipeline.request_drain(video_generation)?;
        let audio_generation = if let Some(worker) = audio_decode_pipeline {
            let generation = playback_generation.advance();
            worker.request_drain(generation)?;
            Some(generation)
        } else {
            None
        };

        Ok(Self {
            video_generation,
            audio_generation,
            video_result: None,
            audio_result: None,
        })
    }

    pub(super) fn poll(
        &mut self,
        video_decode_pipeline: &mut VideoDecodePipeline,
        audio_decode_pipeline: Option<&mut AudioDecodePipeline>,
    ) -> std::result::Result<Option<PlaybackDrainResults>, String> {
        if self.video_result.is_none()
            && let Some(result) = video_decode_pipeline.poll_drain_result(self.video_generation)?
        {
            self.video_result = Some(result);
        }

        if let Some(generation) = self.audio_generation
            && self.audio_result.is_none()
        {
            if let Some(worker) = audio_decode_pipeline {
                if let Some(result) = worker.poll_drain_result(generation)? {
                    self.audio_result = Some(result);
                }
            } else {
                self.audio_generation = None;
            }
        }

        if !self.complete() {
            return Ok(None);
        }

        Ok(Some(PlaybackDrainResults {
            video: self
                .video_result
                .take()
                .expect("video drain result exists when phase is complete"),
            audio: self.audio_result.take(),
        }))
    }

    fn complete(&self) -> bool {
        self.video_result.is_some()
            && (self.audio_generation.is_none() || self.audio_result.is_some())
    }
}
