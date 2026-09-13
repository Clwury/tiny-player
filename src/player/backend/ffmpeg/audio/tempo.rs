use super::super::codec::{AudioTempo, DecodedAudio};
use super::{AudioOutput, AudioQueueItem, AudioQueueState, Ordering, TryLockError, ffi};

impl AudioQueueState {
    /// Decode/pending queues retain original PCM. Stretch only when the AO
    /// worker requests output, so their read-ahead never delays a rate change.
    pub(super) fn pop_filtered(
        &mut self,
        sample_rate: i32,
        channels: i32,
        rate: f64,
        generation: u64,
    ) -> Result<Option<AudioQueueItem>, String> {
        loop {
            if let Some(item) = self.filtered.pop_front() {
                return Ok(Some(item));
            }
            let input = self.items.pop_front();
            if input.is_none() && !self.input_eof {
                return Ok(None);
            }
            if let Some(item) = input.as_ref() {
                self.finish_item(item.samples.len(), item.duration_nsecs);
            }
            let generation = input
                .as_ref()
                .map(|item| item.generation)
                .unwrap_or(generation);
            let tempo = self.tempo.get_or_insert_with(|| {
                AudioTempo::new(
                    sample_rate,
                    channels,
                    ffi::AVRational {
                        num: 1,
                        den: 1_000_000_000,
                    },
                )
            });
            let mut output = Vec::new();
            let mut emit = |audio: DecodedAudio, pts: i64| {
                let start = u64::try_from(pts).map_err(|_| "音频输出变速时间戳无效".to_string())?;
                output.push(AudioQueueItem {
                    samples: audio.samples,
                    start_timeline_nsecs: start,
                    end_timeline_nsecs: start.saturating_add(audio.duration_nsecs),
                    duration_nsecs: audio.duration_nsecs,
                    generation,
                });
                Ok(())
            };
            if let Some(item) = input {
                tempo.process(
                    DecodedAudio {
                        samples: item.samples,
                        duration_nsecs: item.duration_nsecs,
                    },
                    item.start_timeline_nsecs as i64,
                    rate,
                    &mut emit,
                )?;
            } else {
                tempo.set_rate(rate, &mut emit)?;
                tempo.drain(&mut emit)?;
                self.input_eof = false;
            }
            for item in output {
                self.queued_samples = self.queued_samples.saturating_add(item.samples.len());
                self.queued_duration_nsecs = self
                    .queued_duration_nsecs
                    .saturating_add(item.duration_nsecs);
                self.filtered.push_back(item);
            }
        }
    }
}

impl AudioOutput {
    /// EOF is published only after all pending decoded PCM reached this queue.
    /// A short filter tail must drain even if it cannot meet the AO prefill.
    pub(in crate::player::backend::ffmpeg) fn finish_audio_input(&self) -> Result<bool, String> {
        let mut state = match self.queue.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Ok(false),
            Err(TryLockError::Poisoned(_)) => return Err("系统音频解码队列已损坏".to_string()),
        };
        if self.pending_fenced_reset_epoch.load(Ordering::Acquire) != 0 {
            return Ok(false);
        }
        state.input_eof = true;
        drop(state);
        self.queue.ready.notify_all();
        Ok(true)
    }
}
