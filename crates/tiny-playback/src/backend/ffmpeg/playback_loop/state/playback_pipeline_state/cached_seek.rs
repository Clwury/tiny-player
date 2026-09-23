use super::*;

impl PlaybackPipelineState {
    pub(super) fn cached_seek_recovery_next_action(
        &mut self,
        target_nsecs: u64,
        cached_seek: Option<DemuxCachedSeekInfo>,
    ) -> CachedSeekRecoveryFallbackAction {
        cached_seek_recovery_next_action_for_attempt(
            &mut self.cached_seek_recovery_attempt,
            target_nsecs,
            self.video_decode_pipeline.info().hardware_accelerated,
            cached_seek.is_some_and(DemuxCachedSeekInfo::uses_cra_anchor),
        )
    }

    pub(in super::super::super) fn decoder_outputs_pending_or_in_flight(&self) -> bool {
        self.video_decode_pipeline.has_pending_or_in_flight()
            || self
                .audio_decode_pipeline
                .as_ref()
                .is_some_and(|pipeline| pipeline.has_pending_or_in_flight())
            || self.subtitle_pipeline.has_pending_or_in_flight()
    }
}
