use super::{
    AUDIO_CALLBACK_GAP_LOG_AFTER, Arc, AudioClockMode, AudioShared, DeviceTrait, Duration,
    FromSample, Ordering, Sample, SizedSample, audio_elements_duration, audio_frames_for_elements,
    duration_nsecs,
};

pub(in crate::player::backend::ffmpeg::audio) fn build_audio_output_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: Arc<AudioShared>,
) -> std::result::Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let error_callback = |error| tracing::warn!(%error, "native audio output stream error");
    device.build_output_stream(
        config,
        move |data: &mut [T], info| fill_audio_output_with_timing(data, info, &shared),
        error_callback,
        None,
    )
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg) fn fill_audio_output<T>(data: &mut [T], shared: &AudioShared)
where
    T: Sample + FromSample<f32>,
{
    fill_audio_output_samples(data, shared, None);
}

pub(in crate::player::backend::ffmpeg::audio) fn fill_audio_output_with_timing<T>(
    data: &mut [T],
    info: &cpal::OutputCallbackInfo,
    shared: &AudioShared,
) where
    T: Sample + FromSample<f32>,
{
    let timestamp = info.timestamp();
    let playback_delay = timestamp.playback.duration_since(&timestamp.callback);
    fill_audio_output_samples(data, shared, playback_delay);
}

fn fill_audio_output_samples<T>(
    data: &mut [T],
    shared: &AudioShared,
    playback_delay: Option<Duration>,
) where
    T: Sample + FromSample<f32>,
{
    let callback_nsecs = duration_nsecs(shared.clock_start.elapsed());
    let callback_index = shared
        .callback_count
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    let previous_callback_nsecs = shared
        .last_callback_nsecs
        .swap(callback_nsecs, Ordering::Relaxed);
    if previous_callback_nsecs > 0 {
        let callback_gap_nsecs = callback_nsecs.saturating_sub(previous_callback_nsecs);
        if callback_gap_nsecs >= duration_nsecs(AUDIO_CALLBACK_GAP_LOG_AFTER) {
            tracing::debug!(
                callback_index,
                callback_gap_ms = callback_gap_nsecs as f64 / 1_000_000.0,
                output_samples = data.len(),
                "native audio output callback gap exceeded threshold"
            );
        }
    }

    let mut guard = shared.buffer.lock().expect("audio output buffer poisoned");
    if shared.control.should_pause_audio_output() {
        for sample in data.iter_mut() {
            *sample = T::from_sample(0.0);
        }
        tracing::trace!(
            callback_index,
            output_samples = data.len(),
            silence_fill_reason = "paused",
            clock_mode = AudioClockMode::AudioStarted.as_str(),
            misaligned_audio_buffer_count =
                shared.misaligned_audio_buffer_count.load(Ordering::Relaxed),
            "native audio output callback filled silence while paused"
        );
        shared.update_output_delay(Duration::ZERO);
        shared.ready.notify_all();
        return;
    }

    let volume = shared.control.volume();
    let mut played = 0u64;
    let output_samples = data.len();
    let queued_samples_before = guard.len();
    for sample in data {
        let value = match guard.pop_sample() {
            Some(value) => {
                played = played.saturating_add(1);
                value * volume
            }
            None => 0.0,
        }
        .clamp(-1.0, 1.0);
        *sample = T::from_sample(value);
    }
    let queued_samples_after = guard.len();
    drop(guard);

    if played > 0 {
        shared.played_samples.fetch_add(played, Ordering::Relaxed);
        let played_duration = audio_elements_duration(
            usize::try_from(played).unwrap_or(usize::MAX),
            shared.sample_rate,
            shared.channels,
        );
        shared.update_output_delay(
            playback_delay
                .unwrap_or_default()
                .saturating_add(played_duration),
        );
    } else {
        shared.update_output_delay(Duration::ZERO);
    }
    let underrun_samples = output_samples.saturating_sub(usize::try_from(played).unwrap_or(0));
    if underrun_samples > 0 {
        let queued_duration_after_nsecs = duration_nsecs(audio_elements_duration(
            queued_samples_after,
            shared.sample_rate,
            shared.channels,
        ));
        let audio_gap_frames = audio_frames_for_elements(underrun_samples, shared.channels);
        let underrun_timeline_nsecs =
            shared.played_timeline_nsecs_from_pending(queued_duration_after_nsecs);
        let underrun_started = shared.mark_underrun(underrun_timeline_nsecs);
        let underrun_index = shared
            .underrun_count
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if underrun_index == 1 || underrun_index.is_multiple_of(120) {
            tracing::debug!(
                callback_index,
                underrun_count = underrun_index,
                underrun_samples,
                audio_gap_frames,
                played_samples = played,
                output_samples,
                queued_samples_before,
                queued_samples_after,
                underrun_started,
                underrun_timeline_nsecs,
                silence_fill_reason = "underrun",
                clock_mode = AudioClockMode::UnderrunRecovery.as_str(),
                misaligned_audio_buffer_count =
                    shared.misaligned_audio_buffer_count.load(Ordering::Relaxed),
                "native audio output callback filled silence after underrun"
            );
        }
    }
    shared.ready.notify_all();
}
