use super::avio::{CachedAvio, CachedInputSource, input_format_options};
use std::{
    ffi::{CStr, CString},
    os::raw::{c_int, c_void},
    ptr,
    sync::Arc,
};

use ffmpeg_sys_next as ffi;

use crate::player::backend::PlaybackFileInfo;

use super::{
    FfmpegControl, HttpRingCache, InputProbeProfile, ffmpeg_error, ffmpeg_interrupt_callback,
    nsecs_to_timestamp, seconds_to_nsecs, stream_frame_duration_nsecs, timestamp_to_nsecs,
};

pub(super) struct FormatContext {
    ptr: *mut ffi::AVFormatContext,
    _cached_io: Option<CachedAvio>,
}

impl FormatContext {
    pub(super) fn open(
        url: &str,
        http_headers: &[(String, String)],
        probe_profile: InputProbeProfile,
        cached_source: &CachedInputSource,
        control: Arc<FfmpegControl>,
    ) -> std::result::Result<Self, String> {
        let cached_io = cached_source.cached_avio()?;
        Self::open_with_cached_io(url, http_headers, probe_profile, control, cached_io)
    }

    fn open_with_cached_io(
        url: &str,
        http_headers: &[(String, String)],
        probe_profile: InputProbeProfile,
        control: Arc<FfmpegControl>,
        mut cached_io: Option<CachedAvio>,
    ) -> std::result::Result<Self, String> {
        let url = CString::new(url).map_err(|_| "播放地址包含无效字符".to_string())?;
        let mut options = input_format_options(http_headers, probe_profile)?;
        let mut context = unsafe { ffi::avformat_alloc_context() };
        if context.is_null() {
            unsafe { ffi::av_dict_free(&mut options) };
            return Err("FFmpeg 分配输入上下文失败".to_string());
        }

        unsafe {
            (*context).interrupt_callback.callback = Some(ffmpeg_interrupt_callback);
            (*context).interrupt_callback.opaque = Arc::as_ptr(&control) as *mut c_void;
            if let Some(cached_io) = cached_io.as_mut() {
                (*context).pb = cached_io.as_mut_ptr();
                (*context).flags |= ffi::AVFMT_FLAG_CUSTOM_IO;
            }
        }

        let result = unsafe {
            ffi::avformat_open_input(&mut context, url.as_ptr(), ptr::null_mut(), &mut options)
        };
        unsafe { ffi::av_dict_free(&mut options) };
        if result < 0 {
            if !context.is_null() {
                unsafe { ffi::avformat_close_input(&mut context) };
            }
            return Err(format!("FFmpeg 打开媒体失败：{}", ffmpeg_error(result)));
        }

        Ok(Self {
            ptr: context,
            _cached_io: cached_io,
        })
    }

    pub(super) fn find_stream_info(&mut self) -> std::result::Result<(), String> {
        let result = unsafe { ffi::avformat_find_stream_info(self.ptr, ptr::null_mut()) };
        if result < 0 {
            return Err(format!("FFmpeg 探测媒体流失败：{}", ffmpeg_error(result)));
        }
        Ok(())
    }

    pub(super) fn best_stream(
        &self,
        media_type: ffi::AVMediaType,
    ) -> std::result::Result<Option<StreamInfo>, String> {
        let mut decoder: *const ffi::AVCodec = ptr::null();
        let index =
            unsafe { ffi::av_find_best_stream(self.ptr, media_type, -1, -1, &mut decoder, 0) };
        if index == ffi::AVERROR_STREAM_NOT_FOUND || index == ffi::AVERROR_DECODER_NOT_FOUND {
            return Ok(None);
        }
        if index < 0 {
            return Err(format!("FFmpeg 选择媒体流失败：{}", ffmpeg_error(index)));
        }

        let stream = self.stream(index)?;
        let codecpar = unsafe { (*stream).codecpar };
        if codecpar.is_null() {
            return Err("FFmpeg 媒体流缺少 codec 参数".to_string());
        }
        let time_base = unsafe { (*stream).time_base };
        let start_nsecs = unsafe { timestamp_to_nsecs((*stream).start_time, time_base) };
        let frame_duration_nsecs = unsafe { stream_frame_duration_nsecs(stream) };
        Ok(Some(StreamInfo {
            index,
            stream,
            decoder,
            codec_id: unsafe { (*codecpar).codec_id },
            time_base,
            start_nsecs,
            frame_duration_nsecs,
        }))
    }

    pub(super) fn stream_by_index(
        &self,
        index: usize,
        media_type: ffi::AVMediaType,
    ) -> std::result::Result<StreamInfo, String> {
        let stream =
            self.stream(c_int::try_from(index).map_err(|_| "FFmpeg 媒体流索引无效".to_string())?)?;
        let codecpar = unsafe { (*stream).codecpar };
        if codecpar.is_null() {
            return Err("FFmpeg 媒体流缺少 codec 参数".to_string());
        }
        if unsafe { (*codecpar).codec_type } != media_type {
            return Err("FFmpeg 媒体流类型与所选轨道不匹配".to_string());
        }

        let decoder = unsafe { ffi::avcodec_find_decoder((*codecpar).codec_id) };
        if decoder.is_null() {
            return Err("FFmpeg 未找到所选媒体流的解码器".to_string());
        }
        let time_base = unsafe { (*stream).time_base };
        let start_nsecs = unsafe { timestamp_to_nsecs((*stream).start_time, time_base) };
        let frame_duration_nsecs = unsafe { stream_frame_duration_nsecs(stream) };
        Ok(StreamInfo {
            index: c_int::try_from(index).map_err(|_| "FFmpeg 媒体流索引无效".to_string())?,
            stream,
            decoder,
            codec_id: unsafe { (*codecpar).codec_id },
            time_base,
            start_nsecs,
            frame_duration_nsecs,
        })
    }

    pub(super) fn streams(&self) -> std::result::Result<Vec<StreamInfo>, String> {
        let stream_count = unsafe { (*self.ptr).nb_streams as usize };
        let mut streams = Vec::with_capacity(stream_count);
        for index in 0..stream_count {
            let stream = self
                .stream(c_int::try_from(index).map_err(|_| "FFmpeg 媒体流索引无效".to_string())?)?;
            let codecpar = unsafe { (*stream).codecpar };
            if codecpar.is_null() {
                continue;
            }
            let decoder = unsafe { ffi::avcodec_find_decoder((*codecpar).codec_id) };
            let time_base = unsafe { (*stream).time_base };
            let start_nsecs = unsafe { timestamp_to_nsecs((*stream).start_time, time_base) };
            let frame_duration_nsecs = unsafe { stream_frame_duration_nsecs(stream) };
            streams.push(StreamInfo {
                index: c_int::try_from(index).map_err(|_| "FFmpeg 媒体流索引无效".to_string())?,
                stream,
                decoder,
                codec_id: unsafe { (*codecpar).codec_id },
                time_base,
                start_nsecs,
                frame_duration_nsecs,
            });
        }
        Ok(streams)
    }

    fn stream(&self, index: c_int) -> std::result::Result<*mut ffi::AVStream, String> {
        let index = usize::try_from(index).map_err(|_| "FFmpeg 媒体流索引无效".to_string())?;
        let stream_count = unsafe { (*self.ptr).nb_streams as usize };
        if index >= stream_count {
            return Err("FFmpeg 媒体流索引越界".to_string());
        }
        let stream = unsafe { *(*self.ptr).streams.add(index) };
        if stream.is_null() {
            return Err("FFmpeg 媒体流为空".to_string());
        }
        Ok(stream)
    }

    pub(super) fn duration_seconds(&self) -> Option<f64> {
        let duration = unsafe { (*self.ptr).duration };
        if duration <= 0 || duration == ffi::AV_NOPTS_VALUE {
            return None;
        }
        Some(duration as f64 / ffi::AV_TIME_BASE as f64)
    }

    pub(super) fn playback_file_info(&self) -> PlaybackFileInfo {
        let format = unsafe { self.ptr.as_ref().map(|context| context.iformat) }
            .filter(|format| !format.is_null());
        let format_name = format.and_then(|format| unsafe { non_empty_c_string((*format).name) });
        let format_description =
            format.and_then(|format| unsafe { non_empty_c_string((*format).long_name) });
        let bitrate = unsafe { self.ptr.as_ref().map(|context| context.bit_rate) }
            .filter(|bitrate| *bitrate > 0)
            .and_then(|bitrate| u64::try_from(bitrate).ok());

        PlaybackFileInfo {
            format_name,
            format_description,
            bitrate,
        }
    }

    pub(super) fn cached_io_cache(&self) -> Option<HttpRingCache> {
        self._cached_io.as_ref().map(CachedAvio::cache)
    }

    pub(super) fn shutdown_cached_io_on_drop(&mut self) {
        if let Some(cached_io) = self._cached_io.as_mut() {
            cached_io.shutdown_cache_on_drop();
        }
    }

    pub(super) fn seek_stream(
        &mut self,
        stream: StreamInfo,
        position_seconds: f64,
    ) -> std::result::Result<(), String> {
        let target_nsecs =
            seconds_to_nsecs(position_seconds).saturating_add(stream.start_nsecs.unwrap_or(0));
        let timestamp = nsecs_to_timestamp(target_nsecs, stream.time_base);
        tracing::debug!(
            stream_index = stream.index,
            position_seconds,
            target_nsecs,
            timestamp,
            "seeking FFmpeg input stream"
        );
        let result = unsafe {
            ffi::av_seek_frame(self.ptr, stream.index, timestamp, ffi::AVSEEK_FLAG_BACKWARD)
        };
        if result < 0 {
            return Err(format!("FFmpeg 跳转媒体位置失败：{}", ffmpeg_error(result)));
        }
        Ok(())
    }

    pub(super) fn as_mut_ptr(&mut self) -> *mut ffi::AVFormatContext {
        self.ptr
    }
}

unsafe fn non_empty_c_string(value: *const std::os::raw::c_char) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let value = unsafe { CStr::from_ptr(value) }.to_string_lossy();
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

unsafe impl Send for FormatContext {}

impl Drop for FormatContext {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffi::avformat_close_input(&mut self.ptr) };
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct StreamInfo {
    pub(super) index: c_int,
    pub(super) stream: *mut ffi::AVStream,
    pub(super) decoder: *const ffi::AVCodec,
    pub(super) codec_id: ffi::AVCodecID,
    pub(super) time_base: ffi::AVRational,
    pub(super) start_nsecs: Option<u64>,
    pub(super) frame_duration_nsecs: Option<u64>,
}

unsafe impl Send for StreamInfo {}
