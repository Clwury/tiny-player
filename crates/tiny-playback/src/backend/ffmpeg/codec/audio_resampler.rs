use super::*;

impl AudioResampler {
    pub(in super::super) fn new(
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

    pub(in super::super) fn convert(
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
