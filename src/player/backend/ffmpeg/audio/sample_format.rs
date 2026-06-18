use super::{c_int, ffi};

pub(in crate::player::backend::ffmpeg) fn frame_sample_format(
    frame: *mut ffi::AVFrame,
) -> std::result::Result<ffi::AVSampleFormat, String> {
    let format = unsafe { (*frame).format };
    match format {
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_U8 as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_U8)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S16 as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S16)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S32 as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S32)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_DBL as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_DBL)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_U8P as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_U8P)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S16P as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S16P)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S32P as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S32P)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_DBLP as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_DBLP)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S64 as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S64)
        }
        value if value == ffi::AVSampleFormat::AV_SAMPLE_FMT_S64P as c_int => {
            Ok(ffi::AVSampleFormat::AV_SAMPLE_FMT_S64P)
        }
        _ => Err(format!("FFmpeg 音频帧采样格式无效：{format}")),
    }
}

pub(in crate::player::backend::ffmpeg) fn audio_sample_len(
    samples: c_int,
    channels: c_int,
) -> std::result::Result<usize, String> {
    audio_elements_for_frames_checked(samples, channels)
}

pub(in crate::player::backend::ffmpeg) fn audio_elements_for_frames_checked(
    frames: c_int,
    channels: c_int,
) -> std::result::Result<usize, String> {
    if frames < 0 || channels <= 0 {
        return Err("音频帧尺寸无效".to_string());
    }
    usize::try_from(frames)
        .ok()
        .and_then(|frames| frames.checked_mul(usize::try_from(channels).ok()?))
        .ok_or_else(|| "音频帧过大".to_string())
}

pub(in crate::player::backend::ffmpeg) fn zeroed_channel_layout() -> ffi::AVChannelLayout {
    unsafe { std::mem::zeroed() }
}
