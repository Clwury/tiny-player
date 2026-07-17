use super::{
    BackendError, BackendEvent, BackendEventKind, PlaybackCacheConfig, PlaybackCacheMode,
    PlaybackSeekMode, Result,
};
#[cfg(test)]
use super::{ByteCacheState, PlaybackCacheByteRange, PlaybackCacheState};

mod audio;
mod avio;
mod backend;
mod bsf;
mod clock;
mod codec;
mod constants;
mod dovi;
mod format;
mod hw;
mod playback_loop;
mod probe_profile;
mod reporting;
mod subtitle;
mod util;
mod video;
mod worker;

pub use backend::FfmpegBackend;

#[cfg(test)]
use audio::audio_sample_len;
use audio::{
    AudioClockMode, AudioOutput, AudioOutputDrainStatus, AudioOutputPushResult,
    AudioOutputSnapshot, align_audio_elements_to_frame_boundary, audio_elements_for_duration_floor,
    audio_elements_for_frames, audio_frames_for_duration_round,
};
#[cfg(test)]
use avio::{
    CacheReadResult, HttpContentRange, HttpRingCacheState, content_len_from_content_range,
    content_range_from_headers, ffmpeg_http_headers, http_cache_playback_range_request_bytes,
    http_cache_range_header, http_cache_range_request_len, http_cache_range_request_timeout,
    http_cache_request_headers_for_log, http_cache_response_headers_for_log, should_cache_http_url,
};
use avio::{HttpRingCache, reqwest_header_pairs};
use bsf::PgsFrameMergeBitstreamFilter;
#[cfg(test)]
use clock::queued_video_target_reached;
#[cfg(test)]
use clock::{MappedTimestamp, WaitStatus};
use clock::{
    PlaybackScheduler, QueuedVideoFrame, TimestampMapper, duration_nsecs,
    frame_best_effort_timestamp, frame_decode_error_flags, frame_is_corrupt, max_optional_seconds,
    nsecs_to_seconds, nsecs_to_timestamp, optional_buffered_value_changed, pts_distance,
    queued_video_duration, queued_video_frames_have_vulkan, queued_video_limit_duration,
    queued_video_limit_frames, queued_video_limit_reached, queued_video_target_duration,
    queued_video_target_frames, seconds_to_nsecs, stream_frame_duration_nsecs, timestamp_to_nsecs,
};
use codec::{
    AudioResampler, AvFrame, AvPacket, AvPacketReadDiagnostic, AvPacketStorageKind, DecodedAudio,
    Decoder, VideoScaler, audio_codec_requires_recovery_point, packet_is_audio_recovery_point,
    packet_is_video_recovery_point, packet_is_video_seek_point,
};
use constants::*;
use dovi::{DoviPipeline, ffmpeg_dovi_metadata_from_frame};
#[cfg(test)]
use dovi::{dovi_packet_timeline_nsecs, has_annex_b_start_code};
use format::{FormatContext, StreamInfo};
use hw::{
    HardwareDecodeMode, VideoHwDecodeContext, is_vulkan_frame, vulkan_frame_planes,
    vulkan_sw_format,
};
use probe_profile::InputProbeProfile;
use reporting::{BufferedReporter, PositionReporter};
use subtitle::{DecodedSubtitleCue, decoded_subrip_packet_cue, load_external_subtitle_cues};
use util::ffmpeg_error;
#[cfg(test)]
use video::ffmpeg_raw_video_format;
use video::{VideoFrameConvertContext, VideoFrameConverter, frame_size, video_frame_len};
use worker::{
    FfmpegCommand, FfmpegControl, FfmpegPlaybackInput, FfmpegWorker,
    coalesce_playback_seek_commands, drain_playback_commands, ffmpeg_interrupt_callback,
};

fn normalize_playback_volume(volume: f32) -> f32 {
    let volume = if volume.is_finite() {
        volume
    } else {
        DEFAULT_PLAYBACK_VOLUME
    };
    volume.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests;
