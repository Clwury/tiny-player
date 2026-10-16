use std::ptr;

use ffmpeg_sys_next as ffi;

use super::{ffmpeg_error, supports_hw_config};
use crate::backend::ffmpeg::{AvFrame, video::ffmpeg_raw_video_format};

#[cfg(target_os = "linux")]
pub(super) mod vulkan;

fn supports_vaapi(codec: *const ffi::AVCodec) -> bool {
    supports_hw_config(
        codec,
        ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
        ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
    )
}

pub(in crate::backend::ffmpeg) fn find_vaapi_decoder(
    preferred: *const ffi::AVCodec,
) -> Result<*const ffi::AVCodec, String> {
    if preferred.is_null() {
        return Err("FFmpeg 未找到可用的 VA-API 解码器".to_string());
    }
    if supports_vaapi(preferred) {
        return Ok(preferred);
    }
    // av_find_best_stream may prefer a software-only decoder such as libdav1d.
    // Select another decoder for the same codec before probing VA-API support.
    let codec_id = unsafe { (*preferred).id };
    let mut state = ptr::null_mut();
    loop {
        let codec = unsafe { ffi::av_codec_iterate(&mut state) };
        if codec.is_null() {
            return Err("FFmpeg 解码器不支持 VA-API 硬件帧输出".to_string());
        }
        if unsafe { ffi::av_codec_is_decoder(codec) != 0 && (*codec).id == codec_id }
            && supports_vaapi(codec)
        {
            return Ok(codec);
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) fn create_device(
    codec: *const ffi::AVCodec,
) -> Result<crate::render_host::FfmpegAvBufferRef, String> {
    use std::{env, ffi::CString};

    use crate::render_host::FfmpegAvBufferRef;

    if !supports_vaapi(codec) {
        return Err("FFmpeg 解码器不支持 VA-API 硬件帧输出".to_string());
    }
    let device = env::var_os("TINY_VAAPI_DEVICE")
        .filter(|value| !value.is_empty())
        .map(|value| CString::new(value.as_encoded_bytes()))
        .transpose()
        .map_err(|_| "TINY_VAAPI_DEVICE 包含无效的设备路径".to_string())?;
    let mut device_ref = ptr::null_mut();
    let result = unsafe {
        ffi::av_hwdevice_ctx_create(
            &mut device_ref,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            device.as_ref().map_or(ptr::null(), |value| value.as_ptr()),
            ptr::null_mut(),
            0,
        )
    };
    if result < 0 {
        unsafe { ffi::av_buffer_unref(&mut device_ref) };
        return Err(format!(
            "FFmpeg 创建 VA-API 硬解设备失败：code={result}, error={}；请检查 VA-API 驱动、DRM render node 访问权限及 TINY_VAAPI_DEVICE",
            ffmpeg_error(result)
        ));
    }
    let device = FfmpegAvBufferRef::new_ref(device_ref).map_err(|error| error.to_string());
    unsafe { ffi::av_buffer_unref(&mut device_ref) };
    device
}

pub(in crate::backend::ffmpeg) fn download_vaapi_frame(
    frame: *const ffi::AVFrame,
) -> Result<AvFrame, String> {
    download_frame_with(frame, |destination, source| unsafe {
        ffi::av_hwframe_transfer_data(destination, source, 0)
    })
}

fn download_frame_with(
    frame: *const ffi::AVFrame,
    transfer: impl FnOnce(*mut ffi::AVFrame, *const ffi::AVFrame) -> i32,
) -> Result<AvFrame, String> {
    if frame.is_null() || unsafe { (*frame).format } != ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32
    {
        return Err("FFmpeg 帧不是 VA-API 硬件帧".to_string());
    }
    let frames_ref = unsafe { (*frame).hw_frames_ctx };
    if frames_ref.is_null() || unsafe { (*frames_ref).data.is_null() } {
        return Err("FFmpeg VA-API 帧缺少 hw_frames_ctx".to_string());
    }
    let frames = unsafe { &*((*frames_ref).data as *const ffi::AVHWFramesContext) };
    let format = frames.sw_format;
    if ffmpeg_raw_video_format(format as i32).is_none() {
        return Err(format!("FFmpeg VA-API 回传不支持像素格式 {format:?}"));
    }
    let mut downloaded = AvFrame::new()?;
    // Request the original bit depth explicitly instead of allowing FFmpeg
    // to select the first downloadable format (which could be 8-bit NV12).
    unsafe { (*downloaded.as_mut_ptr()).format = format as i32 };
    let result = transfer(downloaded.as_mut_ptr(), frame);
    if result < 0 {
        return Err(format!(
            "FFmpeg 下载 VA-API 视频帧失败：code={result}, error={}",
            ffmpeg_error(result)
        ));
    }
    if unsafe { (*downloaded.as_mut_ptr()).format } != format as i32 {
        return Err("FFmpeg VA-API 回传改变了视频像素格式".to_string());
    }
    let result = unsafe { ffi::av_frame_copy_props(downloaded.as_mut_ptr(), frame) };
    if result < 0 {
        return Err(format!(
            "FFmpeg 复制 VA-API 视频帧属性失败：{}",
            ffmpeg_error(result)
        ));
    }
    Ok(downloaded)
}

#[cfg(test)]
mod tests;
