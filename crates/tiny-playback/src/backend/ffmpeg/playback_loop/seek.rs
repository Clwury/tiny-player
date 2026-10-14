use ffmpeg_sys_next as ffi;

use super::{HEVC_CACHED_SEEK_PREROLL_NSECS, HEVC_SEEK_PREROLL_NSECS, nsecs_to_seconds};

const PAUSED_SEEK_PREVIEW_PREROLL_NSECS: u64 = 1_000_000_000;

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

pub(super) fn paused_seek_preroll_position_seconds(
    codec_id: ffi::AVCodecID,
    position_seconds: f64,
    user_paused: bool,
) -> f64 {
    let seek_position = preroll_seek_position_seconds(codec_id, position_seconds);
    if user_paused {
        // Raw/intra-frame formats can seek directly past every video packet.
        // Read a short preceding interval so EOF can retain the final frame.
        seek_position
            .min((position_seconds - nsecs_to_seconds(PAUSED_SEEK_PREVIEW_PREROLL_NSECS)).max(0.0))
    } else {
        seek_position
    }
}

#[cfg(test)]
mod seek_preroll_tests {
    use ffmpeg_sys_next as ffi;

    use super::{
        SeekPrerollPolicy, paused_seek_preroll_position_seconds, preroll_seek_position_seconds,
        seek_preroll_policy, video_cached_seek_preroll_nsecs, video_seek_preroll_nsecs,
    };

    #[test]
    fn hevc_cached_seek_uses_half_second_preroll_before_safe_point() {
        assert_eq!(
            seek_preroll_policy(ffi::AVCodecID::AV_CODEC_ID_HEVC),
            SeekPrerollPolicy {
                low_level_nsecs: 1_000_000_000,
                cached_nsecs: 500_000_000,
            }
        );
        assert_eq!(
            video_seek_preroll_nsecs(ffi::AVCodecID::AV_CODEC_ID_HEVC),
            1_000_000_000
        );
        assert_eq!(
            video_cached_seek_preroll_nsecs(ffi::AVCodecID::AV_CODEC_ID_HEVC),
            500_000_000
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

    #[test]
    fn paused_raw_seek_reads_before_the_target_without_changing_running_seek_policy() {
        let codec = ffi::AVCodecID::AV_CODEC_ID_RAWVIDEO;
        assert_eq!(paused_seek_preroll_position_seconds(codec, 6.0, true), 5.0);
        assert_eq!(paused_seek_preroll_position_seconds(codec, 6.0, false), 6.0);
        assert_eq!(paused_seek_preroll_position_seconds(codec, 0.25, true), 0.0);
        assert_eq!(
            paused_seek_preroll_position_seconds(ffi::AVCodecID::AV_CODEC_ID_HEVC, 6.0, true),
            5.0
        );
    }
}
