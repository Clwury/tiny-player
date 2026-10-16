use super::*;

impl Decoder {
    pub(in super::super) fn size(&self) -> std::result::Result<RenderSize, String> {
        let (width, height) = unsafe { ((*self.ptr).width, (*self.ptr).height) };
        if width <= 0 || height <= 0 {
            return Err("FFmpeg 解码器未提供有效视频尺寸".to_string());
        }
        Ok(RenderSize {
            width: u32::try_from(width).map_err(|_| "视频宽度无效".to_string())?,
            height: u32::try_from(height).map_err(|_| "视频高度无效".to_string())?,
        })
    }

    pub(in super::super) fn set_skip_nonref_frames(&self, enabled: bool) {
        let skip_frame = if enabled {
            ffi::AVDiscard::AVDISCARD_NONREF
        } else {
            ffi::AVDiscard::AVDISCARD_DEFAULT
        };
        unsafe {
            (*self.ptr).skip_frame = skip_frame;
        }
    }

    pub(in super::super) fn decode_packet<F>(
        &self,
        packet: *const ffi::AVPacket,
        frame: &mut AvFrame,
        mut on_frame: F,
    ) -> std::result::Result<(), String>
    where
        F: FnMut(*mut ffi::AVFrame) -> std::result::Result<(), String>,
    {
        // Match libavcodec's receive-first state machine: exhaust output left by
        // earlier packets before offering more input. EAGAIN from receive is a
        // normal request for the packet below, not decode-failure evidence.
        self.receive_frames(frame, &mut on_frame)?;
        loop {
            let result = unsafe { ffi::avcodec_send_packet(self.ptr, packet) };
            if let Some(error) = self.hardware_format_error(result) {
                return Err(error);
            }
            if result == ffi::AVERROR(ffi::EAGAIN) {
                // A send-side EAGAIN means output must be drained before the
                // exact same packet is retried; the packet is never discarded.
                self.receive_frames(frame, &mut on_frame)?;
                continue;
            }
            if result < 0 {
                if self.continue_after_video_invalid_data(result, "send_packet") {
                    return self.receive_frames(frame, &mut on_frame);
                }
                return Err(format!(
                    "FFmpeg 发送解码包失败：code={result}, error={}",
                    ffmpeg_error(result)
                ));
            }
            return self.receive_frames(frame, &mut on_frame);
        }
    }

    pub(in super::super) fn flush<F>(
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
            return Err(format!(
                "FFmpeg 刷新解码器失败：code={result}, error={}",
                ffmpeg_error(result)
            ));
        }
        self.receive_frames(frame, &mut on_frame)
    }

    pub(in super::super) fn flush_buffers(&self) {
        unsafe { ffi::avcodec_flush_buffers(self.ptr) };
    }

    pub(in super::super) fn decode_subtitle_packet<F>(
        &self,
        packet: *const ffi::AVPacket,
        mut on_cue: F,
    ) -> std::result::Result<(), String>
    where
        F: FnMut(DecodedSubtitleCue) -> std::result::Result<(), String>,
    {
        let mut subtitle = unsafe { mem::zeroed::<ffi::AVSubtitle>() };
        let mut got_subtitle = 0;
        let result = unsafe {
            ffi::avcodec_decode_subtitle2(self.ptr, &mut subtitle, &mut got_subtitle, packet)
        };
        if result < 0 {
            return Err(format!("FFmpeg 解码字幕失败：{}", ffmpeg_error(result)));
        }
        if got_subtitle == 0 {
            return Ok(());
        }

        let cues = decoded_subtitle_cues(
            &subtitle,
            self.size().ok(),
            self.emits_empty_subtitle_cues(),
        );
        unsafe { ffi::avsubtitle_free(&mut subtitle) };
        for cue in cues? {
            on_cue(cue)?;
        }
        Ok(())
    }

    fn emits_empty_subtitle_cues(&self) -> bool {
        unsafe { (*self.ptr).codec_id == ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE }
    }

    pub(in super::super) fn vulkan_device(&self) -> Option<Arc<VulkanDecodeDevice>> {
        self.video_hw
            .as_ref()
            .and_then(VideoHwDecodeContext::device)
    }

    pub(in super::super) fn decoder_name(&self) -> String {
        let decoder = unsafe { (*self.ptr).codec };
        decoder_name(decoder)
    }

    pub(in super::super) fn is_hardware_accelerated(&self) -> bool {
        self.hardware_decode_backend().is_some()
    }

    pub(in super::super) fn hardware_decode_backend(&self) -> Option<HardwareDecodeBackend> {
        self.video_hw.as_ref().map(VideoHwDecodeContext::backend)
    }

    pub(in super::super) fn hardware_decode_mode(&self) -> HardwareDecodeMode {
        self.hardware_decode_mode
    }

    pub(in super::super) fn mapped_surface_counter(
        &self,
    ) -> Option<Arc<std::sync::atomic::AtomicUsize>> {
        self.video_hw
            .as_ref()
            .and_then(VideoHwDecodeContext::mapped_surface_counter)
    }

    fn hardware_format_error(&self, result: c_int) -> Option<String> {
        let selection = self.hw_format_selection.as_ref()?;
        selection
            .negotiation_failed
            .load(Ordering::Acquire)
            .then(|| {
                let backend = if selection.pixel_format == ffi::AVPixelFormat::AV_PIX_FMT_VAAPI {
                    "VA-API"
                } else {
                    "Vulkan"
                };
                format!("FFmpeg {backend} 硬件像素格式协商失败：code={result}, error={}；强制模式不允许回退软件解码", ffmpeg_error(result))
            })
    }

    fn validate_video_frame_format(
        &self,
        frame: *const ffi::AVFrame,
    ) -> std::result::Result<(), String> {
        if let Some(selection) = self.hw_format_selection.as_ref()
            && !selection.allow_software_fallback
            && unsafe { (*frame).format } != selection.pixel_format as c_int
        {
            return Err(format!(
                "FFmpeg 强制硬解未输出要求的硬件帧：expected={:?}, actual={}",
                selection.pixel_format,
                unsafe { (*frame).format }
            ));
        }
        Ok(())
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
            if let Some(error) = self.hardware_format_error(result) {
                frame.unref();
                return Err(error);
            }
            if result == ffi::AVERROR(ffi::EAGAIN) || result == ffi::AVERROR_EOF {
                return Ok(());
            }
            if result < 0 {
                if self.continue_after_video_invalid_data(result, "receive_frame") {
                    // The error belongs to buffered input. Drain again before
                    // offering another packet, as mpv's decoder wrapper does,
                    // so a receive error cannot discard the caller's input.
                    continue;
                }
                return Err(format!(
                    "FFmpeg 接收解码帧失败：code={result}, error={}",
                    ffmpeg_error(result)
                ));
            }

            let frame_result = self
                .validate_video_frame_format(frame.as_mut_ptr())
                .and_then(|()| {
                    if unsafe { (*frame.as_mut_ptr()).format }
                        == ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as c_int
                    {
                        self.video_hw
                            .as_ref()
                            .ok_or_else(|| "VA-API 帧缺少硬解上下文".to_string())?
                            .prepare_vaapi_frame(frame.as_mut_ptr())
                            .and_then(|mut prepared| on_frame(prepared.as_mut_ptr()))
                    } else {
                        on_frame(frame.as_mut_ptr())
                    }
                });
            frame.unref();
            frame_result?;
        }
    }

    fn continue_after_video_invalid_data(&self, result: c_int, operation: &str) -> bool {
        let codec_id = unsafe { (*self.ptr).codec_id };
        if result != ffi::AVERROR_INVALIDDATA
            || self
                .hw_format_selection
                .as_ref()
                .is_some_and(|selection| selection.negotiation_failed.load(Ordering::Acquire))
            || !matches!(
                codec_id,
                ffi::AVCodecID::AV_CODEC_ID_H264 | ffi::AVCodecID::AV_CODEC_ID_HEVC
            )
        {
            return false;
        }
        // mpv's send_packet/decode_frame report a bad packet/frame and keep
        // decoding. Flushing here destroys usable references and waiting for
        // an IDR can skip every recovery point in an open-GOP stream.
        tracing::debug!(
            operation,
            code = result,
            codec = ?codec_id,
            "continuing video decode after invalid data without flushing references"
        );
        true
    }
}

#[cfg(test)]
#[path = "tests/decoder_runtime.rs"]
mod tests;
