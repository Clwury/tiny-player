use super::*;

impl VideoScaler {
    pub(in super::super) fn new_for_frame(
        frame: *const ffi::AVFrame,
        size: RenderSize,
    ) -> std::result::Result<Self, String> {
        let src_format = unsafe { mem::transmute::<c_int, ffi::AVPixelFormat>((*frame).format) };
        Self::new_with_format(size, src_format)
    }

    fn new_with_format(
        size: RenderSize,
        src_format: ffi::AVPixelFormat,
    ) -> std::result::Result<Self, String> {
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

    pub(in super::super) fn convert(
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
