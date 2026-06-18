use super::{
    PlaybackOutputScheduler, PlaybackOutputSnapshot, duration_nsecs, should_block_for_demux_read,
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
        let can_measure_forward = !self.playback_output_state.first_video_frame_pending()
            && !self.playback_output_state.rebuffering();
        let queued_video_forward_nsecs = played_until_nsecs
            .filter(|_| can_measure_forward)
            .and_then(|played_until| self.scheduled_video_queue.forward_nsecs_from(played_until));
        let video_output_low_water = played_until_nsecs.is_some_and(|played_until| {
            can_measure_forward && self.scheduled_video_queue.low_water(played_until)
        });

        PlaybackOutputSnapshot {
            state: self.playback_output_state,
            first_video_frame_pending: self.first_video_frame_pending,
            rebuffering: self.playback_output_state.rebuffering(),
            queued_video_frames: self.scheduled_video_queue.len(),
            queued_video_duration_nsecs,
            queued_video_range_nsecs,
            queued_video_forward_nsecs,
            video_output_low_water,
            pending_start_audio_frames: self.pending_start_audio.len(),
            pending_start_audio_nsecs: duration_nsecs(self.pending_start_audio.buffered_duration()),
            video_output_rebuffer_anchor: self.video_output_rebuffer_anchor,
        }
    }
}

impl PlaybackOutputSnapshot {
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
