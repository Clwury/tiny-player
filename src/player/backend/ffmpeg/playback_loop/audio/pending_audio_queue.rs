use super::*;

#[derive(Default)]
pub(in crate::player::backend::ffmpeg) struct PendingStartAudio {
    frames: VecDeque<PendingStartAudioFrame>,
    buffered_frames: u64,
    buffered_samples: usize,
    buffered_duration_nsecs: u64,
}

pub(in crate::player::backend::ffmpeg) struct PendingStartAudioFrame {
    pub(in crate::player::backend::ffmpeg) samples: Vec<f32>,
    pub(in crate::player::backend::ffmpeg) start_timeline_nsecs: u64,
    pub(in crate::player::backend::ffmpeg) end_timeline_nsecs: u64,
}

impl PendingStartAudioFrame {
    pub(in crate::player::backend::ffmpeg) fn trim_before(
        &mut self,
        timeline_nsecs: u64,
        sample_rate: c_int,
        channels: c_int,
    ) -> bool {
        if timeline_nsecs <= self.start_timeline_nsecs {
            return true;
        }
        if timeline_nsecs >= self.end_timeline_nsecs {
            return false;
        }

        let drop_samples = samples_for_duration(
            timeline_nsecs.saturating_sub(self.start_timeline_nsecs),
            sample_rate,
            channels,
        );
        let Ok(mut drop_samples) = usize::try_from(drop_samples) else {
            return false;
        };
        if channels > 0 {
            let channels = usize::try_from(channels).unwrap_or(1);
            drop_samples = drop_samples.saturating_sub(drop_samples % channels);
        }
        if drop_samples >= self.samples.len() {
            return false;
        }
        if drop_samples > 0 {
            self.samples.drain(..drop_samples);
        }
        self.start_timeline_nsecs = timeline_nsecs;
        true
    }
}

impl PendingStartAudio {
    pub(in crate::player::backend::ffmpeg) fn push(
        &mut self,
        audio: DecodedAudio,
        start_timeline_nsecs: u64,
        end_timeline_nsecs: u64,
    ) {
        if audio.samples.is_empty() || end_timeline_nsecs <= start_timeline_nsecs {
            return;
        }
        let sample_count = audio.samples.len();
        let audio_duration_nsecs = audio.duration_nsecs;
        self.frames.push_back(PendingStartAudioFrame {
            samples: audio.samples,
            start_timeline_nsecs,
            end_timeline_nsecs,
        });
        self.buffered_frames = self.buffered_frames.saturating_add(1);
        self.buffered_samples = self.buffered_samples.saturating_add(sample_count);
        self.buffered_duration_nsecs = self
            .buffered_duration_nsecs
            .saturating_add(audio_duration_nsecs);
        let queued_frames = self.frames.len();
        if self.buffered_duration_nsecs >= duration_nsecs(VIDEO_OUTPUT_REBUFFER_RESUME_DURATION)
            && (queued_frames == 1 || queued_frames.is_multiple_of(60))
        {
            tracing::trace!(
                buffered_frames = queued_frames,
                total_buffered_frames = self.buffered_frames,
                queued_audio_ms = self.buffered_duration_nsecs as f64 / 1_000_000.0,
                start_timeline_nsecs,
                end_timeline_nsecs,
                "buffering decoded FFmpeg audio until output gate resumes"
            );
        }
    }

    pub(in crate::player::backend::ffmpeg) fn clear(&mut self) {
        self.frames.clear();
        self.buffered_frames = 0;
        self.buffered_samples = 0;
        self.buffered_duration_nsecs = 0;
    }

    pub(in crate::player::backend::ffmpeg) fn discard_before(
        &mut self,
        timeline_nsecs: u64,
    ) -> usize {
        let mut dropped = 0usize;
        while self
            .frames
            .front()
            .is_some_and(|frame| frame.end_timeline_nsecs <= timeline_nsecs)
        {
            self.pop_front();
            dropped = dropped.saturating_add(1);
        }
        dropped
    }

    pub(in crate::player::backend::ffmpeg) fn first_start_timeline_nsecs(&self) -> Option<u64> {
        self.frames.front().map(|frame| frame.start_timeline_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn first_start_at_or_after(
        &self,
        timeline_nsecs: u64,
    ) -> Option<u64> {
        self.frames
            .iter()
            .find(|frame| frame.start_timeline_nsecs >= timeline_nsecs)
            .map(|frame| frame.start_timeline_nsecs)
    }

    fn first_end_timeline_nsecs(&self) -> Option<u64> {
        self.frames.front().map(|frame| frame.end_timeline_nsecs)
    }

    pub(in crate::player::backend::ffmpeg) fn buffered_until_from(
        &self,
        timeline_nsecs: u64,
    ) -> Option<u64> {
        let mut buffered_until = None;
        let gap_tolerance_nsecs = duration_nsecs(PENDING_AUDIO_CONTINUITY_TOLERANCE);
        for frame in &self.frames {
            if frame.end_timeline_nsecs <= timeline_nsecs {
                continue;
            }
            let current = buffered_until.unwrap_or(timeline_nsecs);
            if frame.start_timeline_nsecs > current.saturating_add(gap_tolerance_nsecs) {
                break;
            }
            buffered_until = Some(current.max(frame.end_timeline_nsecs));
        }
        buffered_until
    }

    pub(in crate::player::backend::ffmpeg) fn forward_duration_from(
        &self,
        timeline_nsecs: u64,
    ) -> Option<u64> {
        self.buffered_until_from(timeline_nsecs)
            .map(|buffered_until| buffered_until.saturating_sub(timeline_nsecs))
    }

    pub(in crate::player::backend::ffmpeg) fn pop_front_until(
        &mut self,
        end_timeline_nsecs: u64,
    ) -> Option<PendingStartAudioFrame> {
        self.first_end_timeline_nsecs()
            .is_some_and(|frame_end| frame_end <= end_timeline_nsecs)
            .then(|| self.pop_front())
            .flatten()
    }

    fn pop_front(&mut self) -> Option<PendingStartAudioFrame> {
        let frame = self.frames.pop_front()?;
        self.buffered_samples = self.buffered_samples.saturating_sub(frame.samples.len());
        self.buffered_duration_nsecs = self.buffered_duration_nsecs.saturating_sub(
            frame
                .end_timeline_nsecs
                .saturating_sub(frame.start_timeline_nsecs),
        );
        Some(frame)
    }

    pub(in crate::player::backend::ffmpeg) fn push_front_frame(
        &mut self,
        frame: PendingStartAudioFrame,
    ) {
        self.buffered_samples = self.buffered_samples.saturating_add(frame.samples.len());
        self.buffered_duration_nsecs = self.buffered_duration_nsecs.saturating_add(
            frame
                .end_timeline_nsecs
                .saturating_sub(frame.start_timeline_nsecs),
        );
        self.frames.push_front(frame);
    }

    pub(in crate::player::backend::ffmpeg) fn len(&self) -> usize {
        self.frames.len()
    }

    pub(in crate::player::backend::ffmpeg) fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub(in crate::player::backend::ffmpeg) fn buffered_duration(&self) -> Duration {
        Duration::from_nanos(self.buffered_duration_nsecs)
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn queued_samples(&self) -> usize {
        self.buffered_samples
    }
}
