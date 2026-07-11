pub(in crate::player::backend::ffmpeg::audio) use std::{
    collections::VecDeque,
    env,
    os::raw::c_int,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(in crate::player::backend::ffmpeg::audio) use cpal::{
    FromSample, Sample, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
pub(in crate::player::backend::ffmpeg::audio) use ffmpeg_sys_next as ffi;

#[cfg(test)]
pub(in crate::player::backend::ffmpeg::audio) use super::AUDIO_OUTPUT_UNDERRUN_RESUME_DURATION;
pub(in crate::player::backend::ffmpeg::audio) use super::{
    AUDIO_BUFFER_SECONDS, AUDIO_CALLBACK_GAP_LOG_AFTER, AUDIO_OUTPUT_DELAY_LIMIT,
    AUDIO_OUTPUT_QUEUE_LIMIT_DURATION, AUDIO_OUTPUT_STAGE_TIMING_LOG_AFTER,
    AUDIO_OUTPUT_UNDERRUN_CLOCK_RESUME_DURATION, AUDIO_QUEUE_WAIT_LOG_AFTER, FfmpegControl,
    SCHEDULER_POLL_INTERVAL, duration_nsecs,
};

#[path = "audio/device.rs"]
mod device;
#[path = "audio/model.rs"]
mod model;
#[path = "audio/output.rs"]
mod output;
#[path = "audio/queue.rs"]
mod queue;
#[path = "audio/sample_format.rs"]
mod sample_format;
#[path = "audio/stream.rs"]
mod stream;
#[cfg(test)]
#[path = "audio/tests.rs"]
mod tests;
#[path = "audio/timing.rs"]
mod timing;

pub(in crate::player::backend::ffmpeg::audio) use device::output_device_candidates;
pub(super) use model::{
    AudioBuffer, AudioClockMode, AudioOutput, AudioOutputDrainStatus, AudioOutputPushResult,
    AudioOutputSnapshot, AudioShared,
};
pub(in crate::player::backend::ffmpeg::audio) use model::{
    AudioQueueItem, AudioQueueShared, AudioQueueSnapshot, AudioQueueState, AudioQueueWriteError,
    AudioQueueWriteProgress, AudioSharedSnapshot,
};
pub(in crate::player::backend::ffmpeg::audio) use queue::spawn_audio_queue_worker;
#[cfg(test)]
pub(in crate::player::backend::ffmpeg::audio) use queue::write_audio_queue_item;
pub(super) use sample_format::{audio_sample_len, frame_sample_format, zeroed_channel_layout};
pub(in crate::player::backend::ffmpeg::audio) use stream::build_audio_output_stream;
#[cfg(test)]
pub(super) use stream::fill_audio_output;
pub(in crate::player::backend::ffmpeg::audio) use timing::{
    AudioOutputSnapshotTiming, AudioOutputTryPushTimedTiming, audio_elements_duration,
    audio_frames_for_elements, interpolated_audio_timeline_nsecs,
    log_audio_output_reset_clock_timing, log_audio_output_snapshot_timing,
    log_audio_output_try_push_timed_timing, log_audio_queue_snapshot_timing,
    log_audio_shared_reset_clock_timing, log_audio_shared_snapshot_timing,
};
pub(super) use timing::{
    align_audio_elements_to_frame_boundary, audio_elements_for_duration_floor,
    audio_elements_for_frames, audio_frames_for_duration_round,
};
#[cfg(test)]
pub(super) use timing::{
    audio_elements_for_duration_round, audio_samples_duration, samples_for_duration,
};
