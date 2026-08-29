use super::*;

impl Decoder {
    pub(in super::super) fn open(stream: StreamInfo) -> std::result::Result<Self, String> {
        let decoder = find_decoder(stream)?;
        Self::open_with_decoder(stream, decoder, None, HardwareDecodeMode::Off, None)
    }

    pub(in super::super) fn open_audio(stream: StreamInfo) -> std::result::Result<Self, String> {
        Self::open(stream)
    }

    pub(in super::super) fn open_subtitle(
        stream: StreamInfo,
        canvas_size: Option<RenderSize>,
    ) -> std::result::Result<Self, String> {
        let decoder = find_decoder(stream)?;
        Self::open_with_decoder(stream, decoder, None, HardwareDecodeMode::Off, canvas_size)
    }

    pub(in super::super) fn open_video(
        stream: StreamInfo,
        hw_mode: HardwareDecodeMode,
    ) -> std::result::Result<Self, String> {
        let decoder = find_decoder(stream)?;
        let decoder_context = if !hw_mode.should_try_vulkan() {
            Self::open_with_decoder(stream, decoder, None, hw_mode, None)
        } else {
            match VideoHwDecodeContext::try_create(decoder) {
                Ok(video_hw) => {
                    match Self::open_with_decoder(stream, decoder, Some(video_hw), hw_mode, None) {
                        Ok(decoder_context) => {
                            tracing::info!(
                                decoder = %decoder_name(decoder),
                                requested_hw_mode = ?hw_mode,
                                active_hwaccel = true,
                                hw_pixel_format = ?decoder_context
                                    .video_hw
                                    .as_ref()
                                    .map(|hw| hw.pixel_format()),
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
                            Self::open_with_decoder(stream, decoder, None, hw_mode, None)
                        }
                        Err(error) => Err(format!("FFmpeg Vulkan 硬解打开失败：{error}")),
                    }
                }
                Err(error) if hw_mode.allows_fallback() => {
                    tracing::warn!(
                        %error,
                        decoder = %decoder_name(decoder),
                        "FFmpeg Vulkan hardware decode unavailable; falling back to software"
                    );
                    Self::open_with_decoder(stream, decoder, None, hw_mode, None)
                }
                Err(error) => Err(format!("FFmpeg Vulkan 硬解不可用：{error}")),
            }
        }?;
        let active_hwaccel = decoder_context.is_hardware_accelerated();
        if hw_mode == HardwareDecodeMode::ForceVulkan && !active_hwaccel {
            return Err("TINY_HWDEC=force-vulkan 但 FFmpeg 未激活 Vulkan 硬解".to_string());
        }
        tracing::info!(
            decoder = %decoder_name(decoder),
            requested_hw_mode = ?hw_mode,
            active_hwaccel,
            hw_pixel_format = ?decoder_context
                .video_hw
                .as_ref()
                .map(|hw| hw.pixel_format()),
            "resolved FFmpeg video decode mode"
        );
        Ok(decoder_context)
    }

    fn open_with_decoder(
        stream: StreamInfo,
        decoder: *const ffi::AVCodec,
        video_hw: Option<VideoHwDecodeContext>,
        hardware_decode_mode: HardwareDecodeMode,
        subtitle_canvas_size: Option<RenderSize>,
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
            hardware_decode_mode,
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
        let libavcodec_version = unsafe { ffi::avcodec_version() };
        let requested_thread_count = if decoder_context.video_hw.is_none() {
            0
        } else if vulkan_decode_needs_single_thread(
            stream.codec_id,
            decoder_context.video_hw.as_ref(),
        ) {
            1
        } else {
            configured_vulkan_decode_thread_count()
        };
        unsafe {
            (*context).pkt_timebase = stream.time_base;
            (*context).thread_count = requested_thread_count;
            (*context).thread_type = ffi::FF_THREAD_FRAME | ffi::FF_THREAD_SLICE;
            if (*context).codec_type == ffi::AVMediaType::AVMEDIA_TYPE_VIDEO
                && let Some(error_recognition) = video_error_recognition(stream.codec_id)
            {
                (*context).flags &= !(ffi::AV_CODEC_FLAG_OUTPUT_CORRUPT as c_int);
                (*context).flags2 &= !ffi::AV_CODEC_FLAG2_SHOW_ALL;
                (*context).err_recognition |= error_recognition;
            }
        }
        if unsafe { (*context).codec_type } == ffi::AVMediaType::AVMEDIA_TYPE_SUBTITLE
            && let Some(size) = subtitle_canvas_size
            && unsafe { (*context).width <= 0 || (*context).height <= 0 }
        {
            unsafe {
                (*context).width =
                    c_int::try_from(size.width).map_err(|_| "字幕画布宽度无效".to_string())?;
                (*context).height =
                    c_int::try_from(size.height).map_err(|_| "字幕画布高度无效".to_string())?;
            }
            tracing::debug!(
                width = size.width,
                height = size.height,
                "filled missing FFmpeg subtitle decoder canvas size from video stream"
            );
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
            return Err(format!(
                "FFmpeg 打开解码器失败：code={result}, error={}",
                ffmpeg_error(result)
            ));
        }
        tracing::debug!(
            decoder = %decoder_name(decoder),
            requested_hw_mode = ?decoder_context.hardware_decode_mode,
            active_hwaccel = decoder_context.video_hw.is_some(),
            libavcodec_version,
            requested_thread_count,
            thread_count = unsafe { (*context).thread_count },
            active_thread_type = unsafe { (*context).active_thread_type },
            err_recognition = unsafe { (*context).err_recognition },
            hw_pixel_format = ?decoder_context.video_hw.as_ref().map(|hw| hw.pixel_format()),
            "opened FFmpeg decoder"
        );

        Ok(decoder_context)
    }
}
