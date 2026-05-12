use super::*;

pub(super) struct VideoFrameConverter {
    scaler: Option<VideoScaler>,
}

impl VideoFrameConverter {
    pub(super) fn new() -> Self {
        Self { scaler: None }
    }

    pub(super) fn convert(
        &mut self,
        decoder: &Decoder,
        frame: *mut ffi::AVFrame,
        dovi_metadata: Option<DoviFrameMetadata>,
    ) -> std::result::Result<DecodedFrame, String> {
        let size = frame_size(frame)
            .or_else(|| self.scaler.as_ref().map(|scaler| scaler.size))
            .or_else(|| decoder.size().ok())
            .ok_or_else(|| "FFmpeg 视频帧缺少有效尺寸".to_string())?;
        if let Some(raw) = raw_video_frame_from_av_frame(frame, size, dovi_metadata)? {
            return Ok(DecodedFrame {
                size,
                pts: None,
                pixels: FramePixels::RawVideo(raw),
            });
        }

        let scaler = match self.scaler.as_mut() {
            Some(scaler) => scaler,
            None => self.scaler.insert(VideoScaler::new(decoder)?),
        };
        let pixels = scaler.convert(frame)?;
        Ok(DecodedFrame {
            size: scaler.size,
            pts: None,
            pixels: FramePixels::Bgra8(pixels),
        })
    }
}

pub(super) fn raw_video_frame_from_av_frame(
    frame: *mut ffi::AVFrame,
    size: RenderSize,
    dovi_metadata: Option<DoviFrameMetadata>,
) -> std::result::Result<Option<RawVideoFrame>, String> {
    let Some(format) = ffmpeg_raw_video_format(unsafe { (*frame).format }) else {
        return Ok(None);
    };
    let color = frame_color(frame, dovi_metadata.as_ref());
    let range = frame_range(frame);
    let chroma_site = frame_chroma_site(frame);
    let planes = copy_raw_video_planes(frame, format, size)?;
    let metadata = dovi_metadata.map(|dolby_vision| FrameDynamicMetadata {
        dolby_vision: Some(dolby_vision),
    });

    Ok(Some(RawVideoFrame {
        format,
        color,
        range,
        chroma_site,
        metadata,
        planes: RawVideoPlanes::Owned(planes),
    }))
}

pub(super) fn ffmpeg_raw_video_format(format: c_int) -> Option<RawVideoFormat> {
    if format == ffi::AVPixelFormat::AV_PIX_FMT_P010LE as c_int {
        Some(RawVideoFormat::P010Le)
    } else if format == ffi::AVPixelFormat::AV_PIX_FMT_YUV420P10LE as c_int {
        Some(RawVideoFormat::I42010Le)
    } else if format == ffi::AVPixelFormat::AV_PIX_FMT_NV12 as c_int {
        Some(RawVideoFormat::Nv12)
    } else if format == ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as c_int {
        Some(RawVideoFormat::I420)
    } else {
        None
    }
}

pub(super) fn copy_raw_video_planes(
    frame: *mut ffi::AVFrame,
    format: RawVideoFormat,
    size: RenderSize,
) -> std::result::Result<Vec<RawVideoPlane>, String> {
    let mut planes = Vec::with_capacity(format.plane_count());
    for plane_index in 0..format.plane_count() {
        let layout = format
            .plane_layout(size, plane_index)
            .map_err(|error| error.to_string())?;
        let data = unsafe { (*frame).data[plane_index] };
        if data.is_null() {
            return Err("FFmpeg raw 视频帧缺少平面数据".to_string());
        }
        let stride = unsafe { (*frame).linesize[plane_index] };
        if stride <= 0 {
            return Err("FFmpeg raw 视频帧 stride 无效".to_string());
        }
        let stride =
            usize::try_from(stride).map_err(|_| "FFmpeg raw 视频帧 stride 无效".to_string())?;
        if stride < layout.row_len {
            return Err("FFmpeg raw 视频帧 stride 小于行宽".to_string());
        }
        let height = usize::try_from(layout.height).map_err(|_| "视频帧过高".to_string())?;
        let len = layout
            .row_len
            .checked_mul(height)
            .ok_or_else(|| "视频帧平面过大".to_string())?;
        let mut plane = Vec::with_capacity(len);
        for row in 0..height {
            let row_start = row
                .checked_mul(stride)
                .ok_or_else(|| "视频帧平面过大".to_string())?;
            let row_data = unsafe { slice::from_raw_parts(data.add(row_start), layout.row_len) };
            plane.extend_from_slice(row_data);
        }
        planes.push(RawVideoPlane {
            data: plane,
            stride: layout.row_len,
        });
    }
    Ok(planes)
}

pub(super) fn frame_color(
    frame: *mut ffi::AVFrame,
    dovi_metadata: Option<&DoviFrameMetadata>,
) -> FrameColor {
    if dovi_metadata.is_some_and(|metadata| metadata.profile == 5) {
        return FrameColor::DolbyVisionProfile5;
    }

    let (primaries, transfer) = unsafe { ((*frame).color_primaries, (*frame).color_trc) };
    let is_bt2020 = matches!(
        primaries,
        ffi::AVColorPrimaries::AVCOL_PRI_BT2020 | ffi::AVColorPrimaries::AVCOL_PRI_SMPTE432
    );
    let is_hdr_transfer = matches!(
        transfer,
        ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084
            | ffi::AVColorTransferCharacteristic::AVCOL_TRC_ARIB_STD_B67
    );
    if is_bt2020 && is_hdr_transfer {
        FrameColor::Hdr10Bt2020
    } else {
        FrameColor::Sdr
    }
}

pub(super) fn frame_range(frame: *mut ffi::AVFrame) -> RawVideoRange {
    match unsafe { (*frame).color_range } {
        ffi::AVColorRange::AVCOL_RANGE_MPEG => RawVideoRange::Limited,
        ffi::AVColorRange::AVCOL_RANGE_JPEG => RawVideoRange::Full,
        _ => RawVideoRange::Unknown,
    }
}

pub(super) fn frame_chroma_site(frame: *mut ffi::AVFrame) -> RawVideoChromaSite {
    match unsafe { (*frame).chroma_location } {
        ffi::AVChromaLocation::AVCHROMA_LOC_LEFT => RawVideoChromaSite::Left,
        ffi::AVChromaLocation::AVCHROMA_LOC_CENTER => RawVideoChromaSite::Center,
        ffi::AVChromaLocation::AVCHROMA_LOC_TOPLEFT => RawVideoChromaSite::TopLeft,
        ffi::AVChromaLocation::AVCHROMA_LOC_TOP => RawVideoChromaSite::TopCenter,
        _ => RawVideoChromaSite::Unknown,
    }
}

pub(super) fn frame_size(frame: *mut ffi::AVFrame) -> Option<RenderSize> {
    let (width, height) = unsafe { ((*frame).width, (*frame).height) };
    if width <= 0 || height <= 0 {
        return None;
    }
    Some(RenderSize {
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
    })
}

pub(super) fn video_frame_len(size: RenderSize) -> std::result::Result<usize, String> {
    let pixels = size
        .width
        .checked_mul(size.height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "视频帧过大".to_string())?;
    usize::try_from(pixels).map_err(|_| "视频帧过大".to_string())
}
