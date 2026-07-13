use std::{collections::VecDeque, os::raw::c_int, time::Duration};

use super::{
    DecodedAudio, PENDING_AUDIO_CONTINUITY_TOLERANCE, VIDEO_OUTPUT_REBUFFER_RESUME_DURATION,
    align_audio_elements_to_frame_boundary, audio_elements_for_duration_floor, duration_nsecs,
};

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

        let drop_samples = audio_elements_for_duration_floor(
            timeline_nsecs.saturating_sub(self.start_timeline_nsecs),
            sample_rate,
            channels,
        );
        let Ok(mut drop_samples) = usize::try_from(drop_samples) else {
            return false;
        };
        drop_samples = align_audio_elements_to_frame_boundary(drop_samples, channels);
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

    pub(in crate::player::backend::ffmpeg) fn contiguous_range_nsecs(&self) -> Option<(u64, u64)> {
        let start_timeline_nsecs = self.first_start_timeline_nsecs()?;
        let end_timeline_nsecs = self.buffered_until_from(start_timeline_nsecs)?;
        Some((start_timeline_nsecs, end_timeline_nsecs))
    }

    pub(in crate::player::backend::ffmpeg) fn contiguous_duration(&self) -> Duration {
        self.contiguous_range_nsecs()
            .map(|(start, end)| Duration::from_nanos(end.saturating_sub(start)))
            .unwrap_or_default()
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

    pub(in crate::player::backend::ffmpeg) fn timeline_gap_near(
        &self,
        initial_previous_end_nsecs: Option<u64>,
        expected_previous_end_nsecs: u64,
        expected_next_start_nsecs: u64,
        min_gap_nsecs: u64,
        endpoint_tolerance_nsecs: u64,
    ) -> Option<(u64, u64)> {
        matching_audio_timeline_gap(
            initial_previous_end_nsecs,
            self.frames
                .iter()
                .map(|frame| (frame.start_timeline_nsecs, frame.end_timeline_nsecs)),
            expected_previous_end_nsecs,
            expected_next_start_nsecs,
            min_gap_nsecs,
            endpoint_tolerance_nsecs,
        )
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

pub(in crate::player::backend::ffmpeg::playback_loop) fn matching_audio_timeline_gap<I>(
    initial_previous_end_nsecs: Option<u64>,
    frames: I,
    expected_previous_end_nsecs: u64,
    expected_next_start_nsecs: u64,
    min_gap_nsecs: u64,
    endpoint_tolerance_nsecs: u64,
) -> Option<(u64, u64)>
where
    I: IntoIterator<Item = (u64, u64)>,
{
    let mut previous_end_nsecs = initial_previous_end_nsecs;
    for (start_nsecs, end_nsecs) in frames {
        if let Some(previous_end) = previous_end_nsecs
            && let Some(gap_nsecs) = start_nsecs.checked_sub(previous_end)
            && gap_nsecs > min_gap_nsecs
            && previous_end.abs_diff(expected_previous_end_nsecs) <= endpoint_tolerance_nsecs
            && start_nsecs.abs_diff(expected_next_start_nsecs) <= endpoint_tolerance_nsecs
        {
            return Some((previous_end, start_nsecs));
        }
        previous_end_nsecs = Some(
            previous_end_nsecs
                .unwrap_or_default()
                .max(end_nsecs.max(start_nsecs)),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{DecodedAudio, PendingStartAudio, matching_audio_timeline_gap};

    fn decoded_audio(duration_nsecs: u64) -> DecodedAudio {
        DecodedAudio {
            samples: vec![0.0; 4],
            duration_nsecs,
        }
    }

    #[test]
    fn contiguous_duration_stops_at_first_real_gap() {
        let mut pending = PendingStartAudio::default();
        pending.push(decoded_audio(20_000_000), 1_000_000_000, 1_020_000_000);
        pending.push(decoded_audio(20_000_000), 1_020_000_000, 1_040_000_000);
        pending.push(decoded_audio(20_000_000), 1_100_000_000, 1_120_000_000);

        assert_eq!(
            pending.contiguous_range_nsecs(),
            Some((1_000_000_000, 1_040_000_000))
        );
        assert_eq!(pending.contiguous_duration().as_nanos(), 40_000_000);
        assert_eq!(pending.buffered_duration().as_nanos(), 60_000_000);
    }

    #[test]
    fn contiguous_duration_allows_small_timestamp_jitter() {
        let mut pending = PendingStartAudio::default();
        pending.push(decoded_audio(20_000_000), 1_000_000_000, 1_020_000_000);
        pending.push(decoded_audio(20_000_000), 1_024_000_000, 1_044_000_000);

        assert_eq!(pending.contiguous_duration().as_nanos(), 44_000_000);
    }

    #[test]
    fn matching_timeline_gap_requires_both_audio_boundaries_to_align() {
        let frames = [
            (1_000_000_000, 1_032_000_000),
            (1_032_000_000, 1_064_000_000),
            (1_896_000_000, 1_928_000_000),
        ];

        assert_eq!(
            matching_audio_timeline_gap(
                None,
                frames,
                1_064_000_000,
                1_914_000_000,
                200_000_000,
                80_000_000,
            ),
            Some((1_064_000_000, 1_896_000_000))
        );
        assert_eq!(
            matching_audio_timeline_gap(
                None,
                frames,
                1_300_000_000,
                1_914_000_000,
                200_000_000,
                80_000_000,
            ),
            None
        );
    }
}
