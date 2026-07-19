use super::{
    Instant, PlaybackOutputScheduler, PlaybackOutputSnapshot, duration_nsecs,
    should_block_for_demux_read,
};

impl PlaybackOutputScheduler {
    pub(in crate::player::backend::ffmpeg) fn snapshot(&self) -> PlaybackOutputSnapshot {
        self.snapshot_for_played_until(None)
    }

    pub(in crate::player::backend::ffmpeg) fn snapshot_for_played_until(
        &self,
        played_until_nsecs: Option<u64>,
    ) -> PlaybackOutputSnapshot {
        let queued_video_duration_nsecs = self.scheduled_video_queue.duration_nsecs();
        let queued_video_range_nsecs = self.scheduled_video_queue.range_nsecs();
        let first_video_frame_pending = self.playback_output_state.first_video_frame_pending();
        let rebuffering = self.playback_output_state.rebuffering();
        let forward_measure_nsecs = if first_video_frame_pending {
            None
        } else if rebuffering {
            self.video_output_rebuffer_anchor
                .map(|anchor| anchor.timeline_nsecs)
                .or(played_until_nsecs)
        } else {
            played_until_nsecs
        };
        let queued_video_contiguous_forward_nsecs = forward_measure_nsecs
            .and_then(|played_until| self.scheduled_video_queue.forward_nsecs_from(played_until));
        let queued_video_largest_gap_nsecs = self.scheduled_video_queue.largest_gap_nsecs();
        let video_output_low_water = played_until_nsecs.is_some_and(|played_until| {
            !first_video_frame_pending
                && !rebuffering
                && self.scheduled_video_queue.low_water(played_until)
        });
        let recent_coordinator_stall = self
            .scheduled_video_queue
            .recent_coordinator_stall(Instant::now());

        PlaybackOutputSnapshot {
            state: self.playback_output_state,
            first_video_frame_pending: self.first_video_frame_pending,
            rebuffering,
            queued_video_frames: self.scheduled_video_queue.len(),
            queued_video_duration_nsecs,
            queued_video_range_nsecs,
            queued_video_forward_nsecs: queued_video_contiguous_forward_nsecs,
            queued_video_contiguous_forward_nsecs,
            queued_video_largest_gap_nsecs,
            video_output_low_water,
            pending_start_audio_frames: self.pending_start_audio.len(),
            pending_start_audio_nsecs: duration_nsecs(self.pending_start_audio.buffered_duration()),
            video_output_rebuffer_anchor: self.video_output_rebuffer_anchor,
            video_bootstrap_after_seek: self.video_bootstrap_after_seek,
            video_decode_underfill: self.video_decode_underfill,
            rebuffer_empty_audio_output_blocked: self.rebuffer_empty_audio_output_blocked,
            scheduler_dropped_video_frames: self
                .scheduled_video_queue
                .scheduler_dropped_video_frames(),
            recent_coordinator_stall_nsecs: recent_coordinator_stall
                .map(|stall| duration_nsecs(stall.elapsed)),
            recent_coordinator_stall_age_nsecs: recent_coordinator_stall
                .map(|stall| duration_nsecs(stall.age)),
        }
    }
}

impl PlaybackOutputSnapshot {
    pub(in crate::player::backend::ffmpeg) fn queued_video_bootstrap_forward_nsecs(self) -> u64 {
        self.queued_video_contiguous_forward_nsecs
            .or(self.queued_video_forward_nsecs)
            .unwrap_or(self.queued_video_duration_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn waiting_for_demux(self) -> bool {
        !self.first_video_frame_pending && self.queued_video_frames == 0
    }

    pub(in crate::player::backend::ffmpeg) fn underflowing(self) -> bool {
        self.waiting_for_demux() || self.video_output_low_water
    }

    pub(in crate::player::backend::ffmpeg) fn should_wait_for_demux(self) -> bool {
        should_block_for_demux_read(self.state)
    }
}
