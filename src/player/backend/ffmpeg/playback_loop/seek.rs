use ffmpeg_sys_next as ffi;

use super::{HEVC_CACHED_SEEK_PREROLL_NSECS, HEVC_SEEK_PREROLL_NSECS, nsecs_to_seconds};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SeekPrerollPolicy {
    low_level_nsecs: u64,
    cached_nsecs: u64,
}

fn seek_preroll_policy(codec_id: ffi::AVCodecID) -> SeekPrerollPolicy {
    match codec_id {
        ffi::AVCodecID::AV_CODEC_ID_HEVC => SeekPrerollPolicy {
            low_level_nsecs: HEVC_SEEK_PREROLL_NSECS,
            cached_nsecs: HEVC_CACHED_SEEK_PREROLL_NSECS,
        },
        _ => SeekPrerollPolicy::default(),
    }
}

pub(super) fn video_seek_preroll_nsecs(codec_id: ffi::AVCodecID) -> u64 {
    seek_preroll_policy(codec_id).low_level_nsecs
}

pub(super) fn video_cached_seek_preroll_nsecs(codec_id: ffi::AVCodecID) -> u64 {
    seek_preroll_policy(codec_id).cached_nsecs
}

pub(super) fn preroll_seek_position_seconds(
    codec_id: ffi::AVCodecID,
    position_seconds: f64,
) -> f64 {
    let position_seconds = position_seconds.max(0.0);
    let preroll_seconds = nsecs_to_seconds(video_seek_preroll_nsecs(codec_id));
    (position_seconds - preroll_seconds).max(0.0)
}

#[cfg(test)]
mod seek_preroll_tests {
    use ffmpeg_sys_next as ffi;

    use super::{
        SeekPrerollPolicy, preroll_seek_position_seconds, seek_preroll_policy,
        video_cached_seek_preroll_nsecs, video_seek_preroll_nsecs,
    };

    #[test]
    fn hevc_low_level_seek_uses_shorter_preroll_than_cached_seek() {
        assert_eq!(
            seek_preroll_policy(ffi::AVCodecID::AV_CODEC_ID_HEVC),
            SeekPrerollPolicy {
                low_level_nsecs: 1_000_000_000,
                cached_nsecs: 5_000_000_000,
            }
        );
        assert_eq!(
            video_seek_preroll_nsecs(ffi::AVCodecID::AV_CODEC_ID_HEVC),
            1_000_000_000
        );
        assert_eq!(
            video_cached_seek_preroll_nsecs(ffi::AVCodecID::AV_CODEC_ID_HEVC),
            5_000_000_000
        );
        assert!(
            (preroll_seek_position_seconds(ffi::AVCodecID::AV_CODEC_ID_HEVC, 62.36) - 61.36).abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn non_hevc_seek_preroll_policy_has_no_preroll() {
        assert_eq!(
            seek_preroll_policy(ffi::AVCodecID::AV_CODEC_ID_H264),
            SeekPrerollPolicy::default()
        );
        assert_eq!(
            preroll_seek_position_seconds(ffi::AVCodecID::AV_CODEC_ID_H264, 62.36),
            62.36
        );
    }
}
