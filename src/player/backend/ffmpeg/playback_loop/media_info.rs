use std::{ffi::CStr, mem, os::raw::c_int};

use ffmpeg_sys_next as ffi;

use crate::player::backend::{PlaybackAudioInfo, PlaybackVideoInfo};

use super::{AudioOutput, StreamInfo, VideoDecodeWorkerInfo};

pub(super) fn playback_video_info_from_worker(
    video_stream: StreamInfo,
    video_decoder: &VideoDecodeWorkerInfo,
) -> Option<PlaybackVideoInfo> {
    let codec_parameters = codec_parameters(video_stream)?;

    Some(PlaybackVideoInfo {
        codec: codec_name(video_stream.codec_id),
        codec_description: codec_description(video_stream),
        profile: codec_profile(video_stream.codec_id, unsafe {
            (*codec_parameters).profile
        }),
        decoder: video_decoder.decoder_name.clone(),
        size: video_decoder.size?,
        sample_aspect_ratio: rational_pair(unsafe { (*codec_parameters).sample_aspect_ratio }),
        frame_rate: frame_rate_from_duration(video_stream.frame_duration_nsecs),
        pixel_format: pixel_format_name(unsafe { (*codec_parameters).format }),
        color_range: filtered_ffmpeg_name(unsafe {
            ffi::av_color_range_name((*codec_parameters).color_range)
        }),
        chroma_location: filtered_ffmpeg_name(unsafe {
            ffi::av_chroma_location_name((*codec_parameters).chroma_location)
        }),
        color_space: filtered_ffmpeg_name(unsafe {
            ffi::av_color_space_name((*codec_parameters).color_space)
        }),
        color_primaries: filtered_ffmpeg_name(unsafe {
            ffi::av_color_primaries_name((*codec_parameters).color_primaries)
        }),
        color_transfer: filtered_ffmpeg_name(unsafe {
            ffi::av_color_transfer_name((*codec_parameters).color_trc)
        }),
        bitrate: positive_u64(unsafe { (*codec_parameters).bit_rate }),
        hardware_accelerated: video_decoder.hardware_accelerated,
    })
}

pub(super) fn playback_audio_info_from_stream(
    audio_stream: Option<StreamInfo>,
    audio_output: Option<&AudioOutput>,
) -> Option<PlaybackAudioInfo> {
    let audio_stream = audio_stream?;
    let codec_parameters = codec_parameters(audio_stream)?;
    let output_channels = audio_output.and_then(|output| positive_u32(output.channels()));
    let output_sample_rate = audio_output.and_then(|output| positive_u32(output.sample_rate()));

    Some(PlaybackAudioInfo {
        codec: codec_name(audio_stream.codec_id),
        codec_description: codec_description(audio_stream),
        profile: codec_profile(audio_stream.codec_id, unsafe {
            (*codec_parameters).profile
        }),
        decoder: decoder_name(audio_stream),
        channels: positive_u32(unsafe { (*codec_parameters).ch_layout.nb_channels }),
        channel_layout: channel_layout_name(unsafe { &(*codec_parameters).ch_layout }),
        sample_format: sample_format_name(unsafe { (*codec_parameters).format }),
        sample_rate: positive_u32(unsafe { (*codec_parameters).sample_rate }),
        output_channels,
        output_sample_format: audio_output.map(|output| output.sample_format().to_string()),
        output_sample_rate,
        output_device: audio_output.map(|output| output.device_name().to_string()),
        bitrate: positive_u64(unsafe { (*codec_parameters).bit_rate }),
    })
}

fn frame_rate_from_duration(frame_duration_nsecs: Option<u64>) -> Option<f64> {
    let duration = frame_duration_nsecs?;
    if duration == 0 {
        return None;
    }
    Some(1_000_000_000.0 / duration as f64)
}

fn codec_parameters(stream: StreamInfo) -> Option<*const ffi::AVCodecParameters> {
    let codec_parameters = unsafe { stream.stream.as_ref()?.codecpar };
    (!codec_parameters.is_null()).then_some(codec_parameters)
}

fn codec_name(codec_id: ffi::AVCodecID) -> String {
    non_empty_c_string(unsafe { ffi::avcodec_get_name(codec_id) })
        .unwrap_or_else(|| format!("{codec_id:?}"))
}

fn codec_description(stream: StreamInfo) -> Option<String> {
    let decoder = unsafe { stream.decoder.as_ref()? };
    non_empty_c_string(decoder.long_name)
}

fn decoder_name(stream: StreamInfo) -> String {
    unsafe { stream.decoder.as_ref() }
        .and_then(|decoder| non_empty_c_string(decoder.name))
        .unwrap_or_else(|| codec_name(stream.codec_id))
}

fn codec_profile(codec_id: ffi::AVCodecID, profile: c_int) -> Option<String> {
    filtered_ffmpeg_name(unsafe { ffi::avcodec_profile_name(codec_id, profile) })
}

fn pixel_format_name(format: c_int) -> Option<String> {
    if format < ffi::AVPixelFormat::AV_PIX_FMT_NONE as c_int
        || format >= ffi::AVPixelFormat::AV_PIX_FMT_NB as c_int
    {
        return None;
    }
    let format = unsafe { mem::transmute::<c_int, ffi::AVPixelFormat>(format) };
    filtered_ffmpeg_name(unsafe { ffi::av_get_pix_fmt_name(format) })
}

fn sample_format_name(format: c_int) -> Option<String> {
    if format < ffi::AVSampleFormat::AV_SAMPLE_FMT_NONE as c_int
        || format >= ffi::AVSampleFormat::AV_SAMPLE_FMT_NB as c_int
    {
        return None;
    }
    let format = unsafe { mem::transmute::<c_int, ffi::AVSampleFormat>(format) };
    filtered_ffmpeg_name(unsafe { ffi::av_get_sample_fmt_name(format) })
}

fn channel_layout_name(layout: &ffi::AVChannelLayout) -> Option<String> {
    let mut buffer = [0i8; 128];
    let result =
        unsafe { ffi::av_channel_layout_describe(layout, buffer.as_mut_ptr(), buffer.len()) };
    if result < 0 {
        return None;
    }
    non_empty_c_string(buffer.as_ptr())
}

fn rational_pair(value: ffi::AVRational) -> Option<(u32, u32)> {
    Some((positive_u32(value.num)?, positive_u32(value.den)?))
}

fn positive_u32(value: c_int) -> Option<u32> {
    (value > 0).then(|| u32::try_from(value).ok()).flatten()
}

fn positive_u64(value: i64) -> Option<u64> {
    (value > 0).then(|| u64::try_from(value).ok()).flatten()
}

fn filtered_ffmpeg_name(value: *const std::os::raw::c_char) -> Option<String> {
    non_empty_c_string(value).filter(|value| {
        !matches!(
            value.to_ascii_lowercase().as_str(),
            "unknown" | "unspecified" | "reserved"
        )
    })
}

fn non_empty_c_string(value: *const std::os::raw::c_char) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let value = unsafe { CStr::from_ptr(value) }.to_string_lossy();
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}
