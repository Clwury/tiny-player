use super::*;

pub(super) struct Decoder {
    ptr: *mut ffi::AVCodecContext,
    pub(super) stream_index: c_int,
    pub(super) time_base: ffi::AVRational,
    video_hw: Option<VideoHwDecodeContext>,
    hw_format_selection: Option<Box<VideoHwFormatSelection>>,
}

impl Decoder {
    pub(super) fn open(stream: StreamInfo) -> std::result::Result<Self, String> {
        let decoder = find_decoder(stream)?;
        Self::open_with_decoder(stream, decoder, None)
    }

    pub(super) fn open_audio(stream: StreamInfo) -> std::result::Result<Self, String> {
        Self::open(stream)
    }

    pub(super) fn open_video(
        stream: StreamInfo,
        hw_mode: HardwareDecodeMode,
    ) -> std::result::Result<Self, String> {
        let decoder = find_decoder(stream)?;
        if !hw_mode.should_try_vulkan() {
            return Self::open_with_decoder(stream, decoder, None);
        }

        match VideoHwDecodeContext::try_create(decoder) {
            Ok(video_hw) => match Self::open_with_decoder(stream, decoder, Some(video_hw)) {
                Ok(decoder_context) => {
                    tracing::info!(
                        decoder = %decoder_name(decoder),
                        "enabled FFmpeg Vulkan hardware video decoder"
                    );
                    Ok(decoder_context)
                }
                Err(error) if hw_mode.allows_fallback() => {
                    tracing::warn!(
                        %error,
                        decoder = %decoder_name(decoder),
                        "FFmpeg Vulkan decoder open failed; falling back to software"
                    );
                    Self::open_with_decoder(stream, decoder, None)
                }
                Err(error) => Err(format!("FFmpeg Vulkan 硬解打开失败：{error}")),
            },
            Err(error) if hw_mode.allows_fallback() => {
                tracing::warn!(
                    %error,
                    decoder = %decoder_name(decoder),
                    "FFmpeg Vulkan hardware decode unavailable; falling back to software"
                );
                Self::open_with_decoder(stream, decoder, None)
            }
            Err(error) => Err(format!("FFmpeg Vulkan 硬解不可用：{error}")),
        }
    }

    fn open_with_decoder(
        stream: StreamInfo,
        decoder: *const ffi::AVCodec,
        video_hw: Option<VideoHwDecodeContext>,
    ) -> std::result::Result<Self, String> {
        let codecpar = unsafe { (*stream.stream).codecpar };
        if codecpar.is_null() {
            return Err("FFmpeg 媒体流缺少 codec 参数".to_string());
        }

        let context = unsafe { ffi::avcodec_alloc_context3(decoder) };
        if context.is_null() {
            return Err("FFmpeg 分配解码上下文失败".to_string());
        }

        let decoder_context = Self {
            ptr: context,
            stream_index: stream.index,
            time_base: stream.time_base,
            hw_format_selection: video_hw.as_ref().map(|hw| {
                Box::new(VideoHwFormatSelection {
                    pixel_format: hw.pixel_format(),
                })
            }),
            video_hw,
        };
        let result = unsafe { ffi::avcodec_parameters_to_context(context, codecpar) };
        if result < 0 {
            return Err(format!(
                "FFmpeg 复制 codec 参数失败：{}",
                ffmpeg_error(result)
            ));
        }
        unsafe {
            (*context).pkt_timebase = stream.time_base;
            (*context).thread_count = 0;
            (*context).thread_type = ffi::FF_THREAD_FRAME | ffi::FF_THREAD_SLICE;
        }
        if let Some(selection) = decoder_context.hw_format_selection.as_ref() {
            unsafe {
                (*context).opaque = selection.as_ref() as *const VideoHwFormatSelection as *mut _;
                (*context).get_format = Some(select_video_hw_format);
            }
        }
        if let Some(video_hw) = decoder_context.video_hw.as_ref() {
            video_hw.attach_to_decoder(context)?;
        }
        let result = unsafe { ffi::avcodec_open2(context, decoder, ptr::null_mut()) };
        if result < 0 {
            return Err(format!("FFmpeg 打开解码器失败：{}", ffmpeg_error(result)));
        }
        tracing::debug!(
            decoder = %decoder_name(decoder),
            thread_count = unsafe { (*context).thread_count },
            active_thread_type = unsafe { (*context).active_thread_type },
            hw_pixel_format = ?decoder_context.video_hw.as_ref().map(|hw| hw.pixel_format()),
            "opened FFmpeg decoder"
        );

        Ok(decoder_context)
    }

    pub(super) fn size(&self) -> std::result::Result<RenderSize, String> {
        let (width, height) = unsafe { ((*self.ptr).width, (*self.ptr).height) };
        if width <= 0 || height <= 0 {
            return Err("FFmpeg 解码器未提供有效视频尺寸".to_string());
        }
        Ok(RenderSize {
            width: u32::try_from(width).map_err(|_| "视频宽度无效".to_string())?,
            height: u32::try_from(height).map_err(|_| "视频高度无效".to_string())?,
        })
    }

    pub(super) fn decode_packet<F>(
        &self,
        packet: *const ffi::AVPacket,
        frame: &mut AvFrame,
        mut on_frame: F,
    ) -> std::result::Result<(), String>
    where
        F: FnMut(*mut ffi::AVFrame) -> std::result::Result<(), String>,
    {
        loop {
            let result = unsafe { ffi::avcodec_send_packet(self.ptr, packet) };
            if result == ffi::AVERROR(ffi::EAGAIN) {
                self.receive_frames(frame, &mut on_frame)?;
                continue;
            }
            if result < 0 {
                return Err(format!("FFmpeg 发送解码包失败：{}", ffmpeg_error(result)));
            }
            return self.receive_frames(frame, &mut on_frame);
        }
    }

    pub(super) fn flush<F>(
        &self,
        frame: &mut AvFrame,
        on_frame: F,
    ) -> std::result::Result<(), String>
    where
        F: FnMut(*mut ffi::AVFrame) -> std::result::Result<(), String>,
    {
        let mut on_frame = on_frame;
        let result = unsafe { ffi::avcodec_send_packet(self.ptr, ptr::null()) };
        if result < 0 && result != ffi::AVERROR_EOF {
            return Err(format!("FFmpeg 刷新解码器失败：{}", ffmpeg_error(result)));
        }
        self.receive_frames(frame, &mut on_frame)
    }

    pub(super) fn flush_buffers(&self) {
        unsafe { ffi::avcodec_flush_buffers(self.ptr) };
    }

    pub(super) fn vulkan_device(&self) -> Option<Arc<VulkanDecodeDevice>> {
        self.video_hw.as_ref().map(VideoHwDecodeContext::device)
    }

    fn receive_frames<F>(
        &self,
        frame: &mut AvFrame,
        on_frame: &mut F,
    ) -> std::result::Result<(), String>
    where
        F: FnMut(*mut ffi::AVFrame) -> std::result::Result<(), String>,
    {
        loop {
            let result = unsafe { ffi::avcodec_receive_frame(self.ptr, frame.as_mut_ptr()) };
            if result == ffi::AVERROR(ffi::EAGAIN) || result == ffi::AVERROR_EOF {
                return Ok(());
            }
            if result < 0 {
                return Err(format!("FFmpeg 接收解码帧失败：{}", ffmpeg_error(result)));
            }

            let frame_result = on_frame(frame.as_mut_ptr());
            frame.unref();
            frame_result?;
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { (*self.ptr).opaque = ptr::null_mut() };
            unsafe { ffi::avcodec_free_context(&mut self.ptr) };
        }
    }
}

struct VideoHwFormatSelection {
    pixel_format: ffi::AVPixelFormat,
}

unsafe extern "C" fn select_video_hw_format(
    context: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    let selection = unsafe { (*context).opaque as *const VideoHwFormatSelection };
    let Some(pixel_format) =
        (unsafe { selection.as_ref().map(|selection| selection.pixel_format) })
    else {
        return unsafe { ffi::avcodec_default_get_format(context, formats) };
    };

    let mut current = formats;
    while !current.is_null() {
        let candidate = unsafe { *current };
        if candidate == ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            break;
        }
        if candidate == pixel_format {
            return candidate;
        }
        current = unsafe { current.add(1) };
    }

    unsafe { ffi::avcodec_default_get_format(context, formats) }
}

fn find_decoder(stream: StreamInfo) -> std::result::Result<*const ffi::AVCodec, String> {
    let codecpar = unsafe { (*stream.stream).codecpar };
    if codecpar.is_null() {
        return Err("FFmpeg 媒体流缺少 codec 参数".to_string());
    }

    let decoder = if stream.decoder.is_null() {
        unsafe { ffi::avcodec_find_decoder((*codecpar).codec_id) }
    } else {
        stream.decoder
    };
    if decoder.is_null() {
        return Err("FFmpeg 未找到可用解码器".to_string());
    }
    Ok(decoder)
}

fn decoder_name(decoder: *const ffi::AVCodec) -> String {
    let name = unsafe {
        if decoder.is_null() || (*decoder).name.is_null() {
            None
        } else {
            Some(CStr::from_ptr((*decoder).name))
        }
    };
    name.map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<unknown>".to_string())
}

pub(super) struct AvPacket {
    ptr: *mut ffi::AVPacket,
}

impl AvPacket {
    pub(super) fn new() -> std::result::Result<Self, String> {
        let ptr = unsafe { ffi::av_packet_alloc() };
        if ptr.is_null() {
            return Err("FFmpeg 分配 packet 失败".to_string());
        }
        Ok(Self { ptr })
    }

    pub(super) fn as_ptr(&self) -> *const ffi::AVPacket {
        self.ptr
    }

    pub(super) fn as_mut_ptr(&mut self) -> *mut ffi::AVPacket {
        self.ptr
    }

    pub(super) fn stream_index(&self) -> c_int {
        unsafe { (*self.ptr).stream_index }
    }

    pub(super) fn best_timestamp(&self) -> Option<i64> {
        unsafe {
            if (*self.ptr).pts != ffi::AV_NOPTS_VALUE {
                Some((*self.ptr).pts)
            } else if (*self.ptr).dts != ffi::AV_NOPTS_VALUE {
                Some((*self.ptr).dts)
            } else {
                None
            }
        }
    }

    pub(super) fn data(&self) -> Option<&[u8]> {
        let (data, size) = unsafe { ((*self.ptr).data, (*self.ptr).size) };
        if data.is_null() || size <= 0 {
            return None;
        }
        Some(unsafe { slice::from_raw_parts(data, usize::try_from(size).ok()?) })
    }

    pub(super) fn unref(&mut self) {
        unsafe { ffi::av_packet_unref(self.ptr) };
    }
}

impl Drop for AvPacket {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffi::av_packet_free(&mut self.ptr) };
        }
    }
}

pub(super) struct AvFrame {
    ptr: *mut ffi::AVFrame,
}

impl AvFrame {
    pub(super) fn new() -> std::result::Result<Self, String> {
        let ptr = unsafe { ffi::av_frame_alloc() };
        if ptr.is_null() {
            return Err("FFmpeg 分配 frame 失败".to_string());
        }
        Ok(Self { ptr })
    }

    pub(super) fn as_mut_ptr(&mut self) -> *mut ffi::AVFrame {
        self.ptr
    }

    pub(super) fn unref(&mut self) {
        unsafe { ffi::av_frame_unref(self.ptr) };
    }
}

impl Drop for AvFrame {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffi::av_frame_free(&mut self.ptr) };
        }
    }
}

pub(super) struct VideoScaler {
    ptr: *mut ffi::SwsContext,
    pub(super) size: RenderSize,
}

impl VideoScaler {
    pub(super) fn new(decoder: &Decoder) -> std::result::Result<Self, String> {
        let size = decoder.size()?;
        let src_format = unsafe { (*decoder.ptr).pix_fmt };
        let ptr = unsafe {
            ffi::sws_getContext(
                i32::try_from(size.width).map_err(|_| "视频宽度过大".to_string())?,
                i32::try_from(size.height).map_err(|_| "视频高度过大".to_string())?,
                src_format,
                i32::try_from(size.width).map_err(|_| "视频宽度过大".to_string())?,
                i32::try_from(size.height).map_err(|_| "视频高度过大".to_string())?,
                ffi::AVPixelFormat::AV_PIX_FMT_BGRA,
                ffi::SwsFlags::SWS_BILINEAR as c_int,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if ptr.is_null() {
            return Err("FFmpeg 创建视频色彩转换器失败".to_string());
        }
        Ok(Self { ptr, size })
    }

    pub(super) fn convert(
        &mut self,
        frame: *mut ffi::AVFrame,
        buffer_pool: &FrameBufferPool,
    ) -> std::result::Result<PooledBytes, String> {
        let size = frame_size(frame).unwrap_or(self.size);
        if size != self.size {
            return Err("FFmpeg 暂不支持播放中切换视频尺寸".to_string());
        }

        let len = video_frame_len(size)?;
        let mut pixels = buffer_pool.rent(len);
        pixels.resize(len, 0);
        let mut dst_data = [ptr::null_mut(); 4];
        let mut dst_linesize = [0; 4];
        dst_data[0] = pixels.as_mut_ptr();
        dst_linesize[0] = i32::try_from(size.width)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or_else(|| "视频帧 stride 过大".to_string())?;

        let height = i32::try_from(size.height).map_err(|_| "视频高度过大".to_string())?;
        let scaled = unsafe {
            ffi::sws_scale(
                self.ptr,
                (*frame).data.as_ptr() as *const *const u8,
                (*frame).linesize.as_ptr(),
                0,
                height,
                dst_data.as_mut_ptr(),
                dst_linesize.as_mut_ptr(),
            )
        };
        if scaled != height {
            return Err("FFmpeg 转换视频帧失败".to_string());
        }
        Ok(pixels)
    }
}

impl Drop for VideoScaler {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffi::sws_freeContext(self.ptr) };
        }
    }
}

pub(super) struct AudioResampler {
    ptr: *mut ffi::SwrContext,
    output_rate: c_int,
    output_channels: c_int,
    output_layout: ffi::AVChannelLayout,
    input_rate: c_int,
    input_format: Option<ffi::AVSampleFormat>,
    input_channels: c_int,
}

impl AudioResampler {
    pub(super) fn new(
        output_rate: c_int,
        output_channels: c_int,
    ) -> std::result::Result<Self, String> {
        if output_rate <= 0 || output_channels <= 0 {
            return Err("系统音频输出配置无效".to_string());
        }

        let mut output_layout = zeroed_channel_layout();
        unsafe { ffi::av_channel_layout_default(&mut output_layout, output_channels) };

        Ok(Self {
            ptr: ptr::null_mut(),
            output_rate,
            output_channels,
            output_layout,
            input_rate: 0,
            input_format: None,
            input_channels: 0,
        })
    }

    pub(super) fn convert(
        &mut self,
        frame: *mut ffi::AVFrame,
    ) -> std::result::Result<Option<DecodedAudio>, String> {
        let input_samples = unsafe { (*frame).nb_samples };
        if input_samples <= 0 {
            return Ok(None);
        }
        self.ensure_configured(frame)?;

        let output_samples = unsafe {
            ffi::av_rescale_rnd(
                ffi::swr_get_delay(self.ptr, self.input_rate as i64) + input_samples as i64,
                self.output_rate as i64,
                self.input_rate as i64,
                ffi::AVRounding::AV_ROUND_UP,
            )
        };
        if output_samples <= 0 {
            return Ok(None);
        }
        let output_samples =
            c_int::try_from(output_samples).map_err(|_| "音频输出采样数过大".to_string())?;
        let sample_len = audio_sample_len(output_samples, self.output_channels)?;
        let mut samples = vec![0f32; sample_len];
        let mut output_planes = [samples.as_mut_ptr().cast::<u8>()];
        let input_planes = unsafe {
            if !(*frame).extended_data.is_null() {
                (*frame).extended_data as *const *const u8
            } else {
                (*frame).data.as_ptr() as *const *const u8
            }
        };
        let converted = unsafe {
            ffi::swr_convert(
                self.ptr,
                output_planes.as_mut_ptr(),
                output_samples,
                input_planes,
                input_samples,
            )
        };
        if converted < 0 {
            return Err(format!(
                "FFmpeg 转换音频帧失败：{}",
                ffmpeg_error(converted)
            ));
        }
        if converted == 0 {
            return Ok(None);
        }

        samples.truncate(audio_sample_len(converted, self.output_channels)?);
        let duration_nsecs =
            ((converted as u64).saturating_mul(1_000_000_000)) / self.output_rate as u64;
        Ok(Some(DecodedAudio {
            samples,
            duration_nsecs,
        }))
    }

    fn ensure_configured(&mut self, frame: *mut ffi::AVFrame) -> std::result::Result<(), String> {
        let input_rate = unsafe { (*frame).sample_rate };
        if input_rate <= 0 {
            return Err("FFmpeg 音频帧采样率无效".to_string());
        }
        let input_format = frame_sample_format(frame)?;
        let mut fallback_input_layout = zeroed_channel_layout();
        let mut fallback_layout_used = false;
        let (input_layout, input_channels) = unsafe {
            if (*frame).ch_layout.nb_channels > 0
                && ffi::av_channel_layout_check(&(*frame).ch_layout) > 0
            {
                (
                    &(*frame).ch_layout as *const ffi::AVChannelLayout,
                    (*frame).ch_layout.nb_channels,
                )
            } else {
                let channels = (*frame)
                    .ch_layout
                    .nb_channels
                    .max(FALLBACK_AUDIO_OUTPUT_CHANNELS);
                ffi::av_channel_layout_default(&mut fallback_input_layout, channels);
                fallback_layout_used = true;
                (
                    &fallback_input_layout as *const ffi::AVChannelLayout,
                    channels,
                )
            }
        };

        if !self.ptr.is_null()
            && self.input_rate == input_rate
            && self.input_format == Some(input_format)
            && self.input_channels == input_channels
        {
            if fallback_layout_used {
                unsafe { ffi::av_channel_layout_uninit(&mut fallback_input_layout) };
            }
            return Ok(());
        }

        if !self.ptr.is_null() {
            unsafe { ffi::swr_free(&mut self.ptr) };
        }

        let mut next = ptr::null_mut();
        let result = unsafe {
            ffi::swr_alloc_set_opts2(
                &mut next,
                &self.output_layout,
                ffi::AVSampleFormat::AV_SAMPLE_FMT_FLT,
                self.output_rate,
                input_layout,
                input_format,
                input_rate,
                0,
                ptr::null_mut(),
            )
        };
        if fallback_layout_used {
            unsafe { ffi::av_channel_layout_uninit(&mut fallback_input_layout) };
        }
        if result < 0 {
            return Err(format!(
                "FFmpeg 配置音频重采样失败：{}",
                ffmpeg_error(result)
            ));
        }

        let result = unsafe { ffi::swr_init(next) };
        if result < 0 {
            unsafe { ffi::swr_free(&mut next) };
            return Err(format!(
                "FFmpeg 初始化音频重采样失败：{}",
                ffmpeg_error(result)
            ));
        }

        self.ptr = next;
        self.input_rate = input_rate;
        self.input_format = Some(input_format);
        self.input_channels = input_channels;
        tracing::debug!(
            input_rate,
            input_channels,
            ?input_format,
            output_rate = self.output_rate,
            output_channels = self.output_channels,
            "initialized FFmpeg audio resampler from decoded frame"
        );
        Ok(())
    }
}

impl Drop for AudioResampler {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffi::swr_free(&mut self.ptr) };
        }
        unsafe { ffi::av_channel_layout_uninit(&mut self.output_layout) };
    }
}

pub(super) struct DecodedAudio {
    pub(super) samples: Vec<f32>,
    pub(super) duration_nsecs: u64,
}
