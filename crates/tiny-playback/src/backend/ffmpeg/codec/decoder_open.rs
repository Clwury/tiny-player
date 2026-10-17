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
        if hw_mode.allows_fallback() {
            let preferred = find_decoder(stream)?;
            let supports_hardware =
                super::super::hw::find_hardware_decoder(preferred, HardwareDecodeMode::ForceVulkan)
                    .is_ok()
                    || (cfg!(target_os = "linux")
                        && super::super::hw::find_vaapi_decoder(preferred).is_ok());
            if !supports_hardware {
                return Self::open_video_candidate(stream, HardwareDecodeMode::Off, hw_mode, None);
            }
            return Self::open_auto_next(
                stream,
                &mut super::super::hw::AutoDecodePlan::detect(hw_mode),
            );
        }
        Self::open_video_candidate(stream, hw_mode, hw_mode, None)
    }

    pub(in super::super) fn open_auto_next(
        stream: StreamInfo,
        plan: &mut super::super::hw::AutoDecodePlan,
    ) -> Result<Self, String> {
        let mut errors = Vec::new();
        while let Some(candidate) = plan.next() {
            match Self::open_video_candidate(stream, candidate, plan.requested_mode(), Some(plan)) {
                Ok(mut decoder) => {
                    decoder.auto_decode_plan = Some(plan.clone());
                    return Ok(decoder);
                }
                Err(error) => {
                    tracing::warn!(?candidate, %error, "Auto decode candidate failed; trying next path");
                    errors.push(format!("{candidate:?}: {error}"));
                }
            }
        }
        Err(format!("Auto 解码候选已耗尽：{}", errors.join("；")))
    }

    pub(in super::super) fn open_video_candidate(
        stream: StreamInfo,
        candidate: HardwareDecodeMode,
        requested_mode: HardwareDecodeMode,
        plan: Option<&super::super::hw::AutoDecodePlan>,
    ) -> Result<Self, String> {
        let preferred = find_decoder(stream)?;
        let backend = match candidate {
            HardwareDecodeMode::ForceVaapi => "VA-API",
            HardwareDecodeMode::ForceVaapiVulkan => "VA-API/Vulkan",
            _ => "Vulkan",
        };
        let decoder = if candidate.uses_vaapi() {
            super::super::hw::find_vaapi_decoder(preferred)?
        } else if candidate.should_try_hardware() {
            super::super::hw::find_hardware_decoder(preferred, candidate)?
        } else {
            preferred
        };
        let video_hw = if !candidate.should_try_hardware() {
            None
        } else {
            Some(
                if let Some(plan) = plan {
                    VideoHwDecodeContext::try_create_for_auto(decoder, candidate, plan)
                } else if candidate.uses_vaapi() {
                    VideoHwDecodeContext::try_create_vaapi(
                        decoder,
                        candidate == HardwareDecodeMode::ForceVaapiVulkan,
                    )
                } else {
                    VideoHwDecodeContext::try_create(decoder)
                }
                .map_err(|error| format!("FFmpeg {backend} 硬解不可用：{error}"))?,
            )
        };
        let decoder_context =
            Self::open_with_decoder(stream, decoder, video_hw, requested_mode, None)?;
        let active_hwaccel = decoder_context.is_hardware_accelerated();
        if candidate.should_try_hardware() && !active_hwaccel {
            return Err(format!(
                "已强制使用 {backend} 硬件解码，但 FFmpeg 未激活硬解"
            ));
        }
        tracing::info!(
            decoder = %decoder_name(decoder),
            requested_hw_mode = ?requested_mode,
            ?candidate,
            backend = ?decoder_context.hardware_decode_backend(),
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
            auto_decode_plan: None,
            hw_format_selection: video_hw.as_ref().map(|hw| {
                Box::new(VideoHwFormatSelection {
                    pixel_format: hw.pixel_format(),
                    // Auto owns the candidate chain. Do not let FFmpeg silently
                    // select software and skip the remaining hardware paths.
                    allow_software_fallback: false,
                    negotiation_failed: AtomicBool::new(false),
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
        let requested_thread_count = if decoder_context.hardware_decode_backend()
            == Some(HardwareDecodeBackend::VaapiVulkan)
        {
            // Reserve surfaces for asynchronous rendering and the codec DPB,
            // including VA-API drivers with a 32-picture limit.
            1
        } else if !decoder_context
            .video_hw
            .as_ref()
            .is_some_and(|hw| hw.pixel_format() == ffi::AVPixelFormat::AV_PIX_FMT_VULKAN)
        {
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
            if (*context).codec_type == ffi::AVMediaType::AVMEDIA_TYPE_VIDEO {
                // Keep libavcodec's default error recognition, like mpv's
                // vd_lavc. EXPLODE makes a malformed HEVC SEI abort the whole
                // access unit before its valid slice can become a reference.
                (*context).flags &= !(ffi::AV_CODEC_FLAG_OUTPUT_CORRUPT as c_int);
                (*context).flags2 &= !ffi::AV_CODEC_FLAG2_SHOW_ALL;
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
