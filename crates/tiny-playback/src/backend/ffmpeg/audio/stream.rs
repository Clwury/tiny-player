use super::{
    AUDIO_CALLBACK_GAP_LOG_AFTER, Arc, AudioClockMode, AudioOutputDecision, AudioOutputLifecycle,
    AudioShared, DeviceTrait, Duration, FromSample, Ordering, Sample, SizedSample,
    audio_elements_duration, audio_frames_for_elements, duration_nsecs,
};

pub(in crate::backend::ffmpeg::audio) fn build_audio_output_stream<T>(
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
pub(in crate::backend::ffmpeg) fn fill_audio_output<T>(data: &mut [T], shared: &AudioShared)
where
    T: Sample + FromSample<f32>,
{
    fill_audio_output_samples(data, shared, None, || {});
}

#[cfg(test)]
pub(in crate::backend::ffmpeg::audio) fn fill_audio_output_with_publish_hook_for_test<T>(
    data: &mut [T],
    shared: &AudioShared,
    before_publish: impl FnOnce(),
) where
    T: Sample + FromSample<f32>,
{
    fill_audio_output_samples(data, shared, None, before_publish);
}

pub(in crate::backend::ffmpeg::audio) fn fill_audio_output_with_timing<T>(
    data: &mut [T],
    info: &cpal::OutputCallbackInfo,
    shared: &AudioShared,
) where
    T: Sample + FromSample<f32>,
{
    let timestamp = info.timestamp();
    let playback_delay = timestamp.playback.duration_since(&timestamp.callback);
    fill_audio_output_samples(data, shared, playback_delay, || {});
}

fn fill_audio_output_samples<T>(
    data: &mut [T],
    shared: &AudioShared,
    playback_delay: Option<Duration>,
    before_publish: impl FnOnce(),
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

    let mut guard = shared
        .buffer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let callback_epoch = guard.epoch;
    let output_state = shared.control.audio_output_control_snapshot();
    let mut waiting_for_prefill = false;
    if output_state.decision() != AudioOutputDecision::Silence
        && shared.timeline.active()
        && shared.timeline.is_current_epoch(callback_epoch)
        && shared.underrun_active.load(Ordering::Acquire)
    {
        let pending_nsecs = shared.buffer_media_duration_nsecs(&guard);
        if output_state.lifecycle() == AudioOutputLifecycle::Draining {
            shared.clear_underrun();
        } else {
            shared.clear_underrun_if_recovered(pending_nsecs);
        }
        waiting_for_prefill = shared.underrun_active.load(Ordering::Acquire);
        if !waiting_for_prefill {
            tracing::debug!(
                callback_index,
                prefill_ms = pending_nsecs as f64 / 1_000_000.0,
                prefill_target_ms =
                    shared.underrun_resume_nsecs.load(Ordering::Acquire) as f64 / 1_000_000.0,
                input_eof = output_state.lifecycle() == AudioOutputLifecycle::Draining,
                "resumed native audio consumption after output prefill"
            );
        }
    }
    if output_state.decision() == AudioOutputDecision::Silence
        || !shared.timeline.active()
        || !shared.timeline.is_current_epoch(callback_epoch)
        || waiting_for_prefill
    {
        for sample in data.iter_mut() {
            *sample = T::from_sample(0.0);
        }
        guard.device_timing.clear();
        drop(guard);
        let mut silenced_callback_count = shared.silenced_callback_count.load(Ordering::Relaxed);
        if let Ok(_publish_guard) = shared.callback_publish_guard.try_lock() {
            if shared.timeline.is_current_epoch(callback_epoch) {
                let _mutation = shared.timeline.begin_mutation();
                silenced_callback_count = shared
                    .silenced_callback_count
                    .fetch_add(1, Ordering::Relaxed)
                    .saturating_add(1);
                shared.update_output_delay_unfenced(Duration::ZERO);
            } else {
                shared.timeline.record_stale_callback_publication();
            }
        }
        tracing::trace!(
            callback_index,
            silenced_callback_count,
            output_samples = data.len(),
            audio_output_lifecycle = output_state.lifecycle().as_str(),
            paused_by_user = output_state.paused_by_user(),
            paused_by_cache = output_state.paused_by_cache(),
            paused_by_rebuffer = output_state.paused_by_rebuffer(),
            paused_by_seek_transition = output_state.paused_by_seek_transition(),
            silence_fill_reason = if waiting_for_prefill {
                "audio_output_prefill"
            } else {
                "audio_output_state"
            },
            clock_mode = AudioClockMode::AudioStarted.as_str(),
            misaligned_audio_buffer_count =
                shared.misaligned_audio_buffer_count.load(Ordering::Relaxed),
            "native audio output callback retained payload while output is held"
        );
        shared.ready.notify_all();
        return;
    }

    let _callback_mutation = shared.timeline.begin_mutation();
    let volume = shared.control.volume();
    let mut played = 0u64;
    let output_samples = data.len();
    let queued_samples_before = guard.len();
    let queued_duration_before_nsecs = shared.buffer_media_duration_nsecs(&guard);
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
    guard.consume_timing(
        played as usize,
        callback_nsecs.saturating_add(duration_nsecs(playback_delay.unwrap_or_default())),
        callback_nsecs,
        shared.sample_rate,
        shared.channels,
    );
    let queued_duration_after_nsecs = shared.buffer_media_duration_nsecs(&guard);
    let queued_end_nsecs = shared.queued_end_timeline_nsecs.load(Ordering::Acquire);
    let device_delay_nsecs = guard.device_delay_nsecs(duration_nsecs(shared.clock_start.elapsed()));
    drop(guard);
    before_publish();

    let Ok(_publish_guard) = shared.callback_publish_guard.try_lock() else {
        shared.ready.notify_all();
        return;
    };
    if !shared.timeline.is_current_epoch(callback_epoch) {
        shared.timeline.record_stale_callback_publication();
        shared.ready.notify_all();
        return;
    }
    if played > 0 {
        shared.update_callback_playback_rate(
            played as usize,
            queued_duration_before_nsecs.saturating_sub(queued_duration_after_nsecs),
        );
        shared
            .consumed_callback_count
            .fetch_add(1, Ordering::Relaxed);
        shared.played_samples.fetch_add(played, Ordering::Relaxed);
        let played_duration = audio_elements_duration(
            usize::try_from(played).unwrap_or(usize::MAX),
            shared.sample_rate,
            shared.channels,
        );
        shared.update_output_delay_unfenced(
            playback_delay
                .unwrap_or_default()
                .saturating_add(played_duration),
        );
    } else {
        shared.update_output_delay_unfenced(Duration::ZERO);
    }
    // Use the same buffer observation as the consumption. A producer may
    // append another rate segment after releasing the ring lock.
    let played_timeline_nsecs = queued_end_nsecs
        .saturating_sub(queued_duration_after_nsecs)
        .saturating_sub(device_delay_nsecs.unwrap_or_else(|| shared.fallback_output_delay_nsecs()));
    let underrun_samples = output_samples.saturating_sub(usize::try_from(played).unwrap_or(0));
    if underrun_samples > 0 {
        let audio_gap_frames = audio_frames_for_elements(underrun_samples, shared.channels);
        let underrun_timeline_nsecs = played_timeline_nsecs;
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
    shared.published_played_timeline_nsecs.store(
        if shared.underrun_active.load(Ordering::Acquire) {
            shared.underrun_timeline_nsecs.load(Ordering::Acquire)
        } else {
            played_timeline_nsecs
        },
        Ordering::Release,
    );
    shared.ready.notify_all();
}
