use super::avio::{CachedAvio, input_format_options, should_cache_http_url};
use super::*;

pub(super) struct FormatContext {
    ptr: *mut ffi::AVFormatContext,
    _cached_io: Option<CachedAvio>,
}

impl FormatContext {
    pub(super) fn open(
        url: &str,
        http_headers: &[(String, String)],
        content_len_hint: Option<u64>,
        probe_profile: InputProbeProfile,
        control: Arc<FfmpegControl>,
        event_tx: Sender<BackendEvent>,
    ) -> std::result::Result<Self, String> {
        let mut cached_io = if should_cache_http_url(url) {
            Some(CachedAvio::new(
                url,
                http_headers,
                content_len_hint,
                Arc::clone(&control),
                event_tx,
            )?)
        } else {
            None
        };
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

    pub(super) fn seek_stream(
        &mut self,
        stream: StreamInfo,
        position_seconds: f64,
    ) -> std::result::Result<(), String> {
        let target_nsecs =
            seconds_to_nsecs(position_seconds).saturating_add(stream.start_nsecs.unwrap_or(0));
        let timestamp = nsecs_to_timestamp(target_nsecs, stream.time_base);
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
