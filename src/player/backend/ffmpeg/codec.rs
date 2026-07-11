use std::{
    ffi::CStr,
    mem,
    os::raw::{c_int, c_uint},
    ptr, slice,
    sync::Arc,
};

use ffmpeg_sys_next as ffi;

use crate::player::render_host::{FrameBufferPool, PooledBytes, RenderSize, VulkanDecodeDevice};

use super::audio::{audio_sample_len, frame_sample_format, zeroed_channel_layout};
use super::subtitle::{DecodedSubtitleCue, decoded_subtitle_cues};
use super::{
    FALLBACK_AUDIO_OUTPUT_CHANNELS, HardwareDecodeMode, StreamInfo, VideoHwDecodeContext,
    ffmpeg_error, frame_size, video_frame_len,
};

const VULKAN_THREAD_SAFE_LIBAVCODEC_VERSION: c_uint = av_version_int(62, 11, 100);

pub(super) struct Decoder {
    ptr: *mut ffi::AVCodecContext,
    pub(super) stream_index: c_int,
    pub(super) time_base: ffi::AVRational,
    video_hw: Option<VideoHwDecodeContext>,
    hw_format_selection: Option<Box<VideoHwFormatSelection>>,
}

// Decoder is moved into a dedicated worker thread and then accessed only from that thread.
unsafe impl Send for Decoder {}

impl Decoder {
    pub(super) fn open(stream: StreamInfo) -> std::result::Result<Self, String> {
        let decoder = find_decoder(stream)?;
        Self::open_with_decoder(stream, decoder, None, None)
    }

    pub(super) fn open_audio(stream: StreamInfo) -> std::result::Result<Self, String> {
        Self::open(stream)
    }

    pub(super) fn open_subtitle(
        stream: StreamInfo,
        canvas_size: Option<RenderSize>,
    ) -> std::result::Result<Self, String> {
        let decoder = find_decoder(stream)?;
        Self::open_with_decoder(stream, decoder, None, canvas_size)
    }

    pub(super) fn open_video(
        stream: StreamInfo,
        hw_mode: HardwareDecodeMode,
    ) -> std::result::Result<Self, String> {
        let decoder = find_decoder(stream)?;
        if !hw_mode.should_try_vulkan() {
            return Self::open_with_decoder(stream, decoder, None, None);
        }

        match VideoHwDecodeContext::try_create(decoder) {
            Ok(video_hw) => match Self::open_with_decoder(stream, decoder, Some(video_hw), None) {
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
                    Self::open_with_decoder(stream, decoder, None, None)
                }
                Err(error) => Err(format!("FFmpeg Vulkan 硬解打开失败：{error}")),
            },
            Err(error) if hw_mode.allows_fallback() => {
                tracing::warn!(
                    %error,
                    decoder = %decoder_name(decoder),
                    "FFmpeg Vulkan hardware decode unavailable; falling back to software"
                );
                Self::open_with_decoder(stream, decoder, None, None)
            }
            Err(error) => Err(format!("FFmpeg Vulkan 硬解不可用：{error}")),
        }
    }

    fn open_with_decoder(
        stream: StreamInfo,
        decoder: *const ffi::AVCodec,
        video_hw: Option<VideoHwDecodeContext>,
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
            (*context).thread_count = if vulkan_decode_needs_single_thread(
                stream.codec_id,
                decoder_context.video_hw.as_ref(),
            ) {
                1
            } else {
                0
            };
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
            return Err(format!("FFmpeg 打开解码器失败：{}", ffmpeg_error(result)));
        }
        tracing::debug!(
            decoder = %decoder_name(decoder),
            thread_count = unsafe { (*context).thread_count },
            active_thread_type = unsafe { (*context).active_thread_type },
            err_recognition = unsafe { (*context).err_recognition },
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

    pub(super) fn set_skip_nonref_frames(&self, enabled: bool) {
        let skip_frame = if enabled {
            ffi::AVDiscard::AVDISCARD_NONREF
        } else {
            ffi::AVDiscard::AVDISCARD_DEFAULT
        };
        unsafe {
            (*self.ptr).skip_frame = skip_frame;
        }
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

    pub(super) fn decode_subtitle_packet<F>(
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

    pub(super) fn vulkan_device(&self) -> Option<Arc<VulkanDecodeDevice>> {
        self.video_hw.as_ref().map(VideoHwDecodeContext::device)
    }

    pub(super) fn decoder_name(&self) -> String {
        let decoder = unsafe { (*self.ptr).codec };
        decoder_name(decoder)
    }

    pub(super) fn is_hardware_accelerated(&self) -> bool {
        self.video_hw.is_some()
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

const fn av_version_int(major: c_uint, minor: c_uint, micro: c_uint) -> c_uint {
    (major << 16) | (minor << 8) | micro
}

fn vulkan_decode_needs_single_thread(
    codec_id: ffi::AVCodecID,
    video_hw: Option<&VideoHwDecodeContext>,
) -> bool {
    video_hw.is_some()
        && vulkan_decode_codec_needs_single_thread(codec_id, unsafe { ffi::avcodec_version() })
}

fn vulkan_decode_codec_needs_single_thread(
    codec_id: ffi::AVCodecID,
    avcodec_version: c_uint,
) -> bool {
    codec_id == ffi::AVCodecID::AV_CODEC_ID_HEVC
        || avcodec_version < VULKAN_THREAD_SAFE_LIBAVCODEC_VERSION
}

fn video_error_recognition(codec_id: ffi::AVCodecID) -> Option<c_int> {
    match codec_id {
        ffi::AVCodecID::AV_CODEC_ID_H264 => {
            Some(ffi::AV_EF_BITSTREAM | ffi::AV_EF_BUFFER | ffi::AV_EF_EXPLODE)
        }
        ffi::AVCodecID::AV_CODEC_ID_HEVC => Some(ffi::AV_EF_BITSTREAM | ffi::AV_EF_BUFFER),
        _ => None,
    }
}

pub(super) fn packet_is_video_recovery_point(packet: &AvPacket, codec_id: ffi::AVCodecID) -> bool {
    match codec_id {
        ffi::AVCodecID::AV_CODEC_ID_H264 => packet
            .data()
            .map(|data| h264_access_unit_recovery_state(data).unwrap_or_else(|| packet.is_key()))
            .unwrap_or_else(|| packet.is_key()),
        ffi::AVCodecID::AV_CODEC_ID_HEVC => packet
            .data()
            .map(|data| hevc_access_unit_recovery_state(data).unwrap_or_else(|| packet.is_key()))
            .unwrap_or_else(|| packet.is_key()),
        _ => packet.is_key(),
    }
}

pub(super) fn packet_is_video_seek_point(packet: &AvPacket, codec_id: ffi::AVCodecID) -> bool {
    if !packet.is_key() {
        return false;
    }

    match codec_id {
        ffi::AVCodecID::AV_CODEC_ID_H264 => packet
            .data()
            .map(|data| h264_access_unit_recovery_state(data).unwrap_or(true))
            .unwrap_or(true),
        ffi::AVCodecID::AV_CODEC_ID_HEVC => packet
            .data()
            .map(|data| hevc_access_unit_seek_state(data).unwrap_or(false))
            .unwrap_or(false),
        _ => true,
    }
}

pub(super) fn audio_codec_requires_recovery_point(codec_id: ffi::AVCodecID) -> bool {
    matches!(
        codec_id,
        ffi::AVCodecID::AV_CODEC_ID_TRUEHD | ffi::AVCodecID::AV_CODEC_ID_MLP
    )
}

pub(super) fn packet_is_audio_recovery_point(packet: &AvPacket, codec_id: ffi::AVCodecID) -> bool {
    if !audio_codec_requires_recovery_point(codec_id) {
        return false;
    }
    packet.data().is_some_and(|data| {
        data.windows(4)
            .any(|window| matches!(window, [0xf8, 0x72, 0x6f, 0xba] | [0xf8, 0x72, 0x6f, 0xbb]))
    })
}

fn h264_access_unit_recovery_state(data: &[u8]) -> Option<bool> {
    access_unit_nal_recovery_state(data, |nal| {
        nal.first()
            .is_some_and(|header| header & 0x1f == H264_NAL_IDR)
    })
}

fn hevc_access_unit_recovery_state(data: &[u8]) -> Option<bool> {
    access_unit_nal_recovery_state(data, |nal| {
        nal.first()
            .map(|header| hevc_nal_is_recovery_point(hevc_nal_type(*header)))
            .unwrap_or(false)
    })
}

fn hevc_access_unit_seek_state(data: &[u8]) -> Option<bool> {
    let mut found_safe_seek_vcl = false;
    let mut found_unsafe_vcl = false;
    access_unit_nal_recovery_state(data, |nal| {
        if let Some(header) = nal.first() {
            let nal_type = hevc_nal_type(*header);
            if hevc_nal_is_vcl(nal_type) {
                if hevc_nal_is_safe_seek_point(nal_type) {
                    found_safe_seek_vcl = true;
                } else {
                    found_unsafe_vcl = true;
                }
            }
        }
        false
    })
    .map(|_| found_safe_seek_vcl && !found_unsafe_vcl)
}

const H264_NAL_IDR: u8 = 5;
const HEVC_NAL_BLA_W_LP: u8 = 16;
const HEVC_NAL_BLA_W_RADL: u8 = 17;
const HEVC_NAL_BLA_N_LP: u8 = 18;
const HEVC_NAL_IDR_W_RADL: u8 = 19;
const HEVC_NAL_IDR_N_LP: u8 = 20;
const HEVC_NAL_CRA: u8 = 21;

fn hevc_nal_type(header: u8) -> u8 {
    (header >> 1) & 0x3f
}

fn hevc_nal_is_vcl(nal_type: u8) -> bool {
    nal_type <= 31
}

fn hevc_nal_is_recovery_point(nal_type: u8) -> bool {
    hevc_nal_is_safe_seek_point(nal_type) || nal_type == HEVC_NAL_CRA
}

fn hevc_nal_is_safe_seek_point(nal_type: u8) -> bool {
    matches!(
        nal_type,
        HEVC_NAL_BLA_W_LP
            | HEVC_NAL_BLA_W_RADL
            | HEVC_NAL_BLA_N_LP
            | HEVC_NAL_IDR_W_RADL
            | HEVC_NAL_IDR_N_LP
    )
}

fn access_unit_nal_recovery_state(
    data: &[u8],
    mut matches_nal: impl FnMut(&[u8]) -> bool,
) -> Option<bool> {
    if access_unit_starts_with_annex_b_start_code(data)
        && let Some(result) = access_unit_has_annex_b_nal(data, &mut matches_nal)
    {
        return Some(result);
    }

    for length_size in [4, 3, 2, 1] {
        match access_unit_has_length_prefixed_nal(data, length_size, &mut matches_nal) {
            Some(result) => return Some(result),
            None => continue,
        }
    }

    if !access_unit_starts_with_annex_b_start_code(data) {
        return access_unit_has_annex_b_nal(data, &mut matches_nal);
    }

    None
}

fn access_unit_has_annex_b_nal(
    data: &[u8],
    matches_nal: &mut impl FnMut(&[u8]) -> bool,
) -> Option<bool> {
    let mut cursor = 0;
    let mut found_start_code = false;
    while let Some((start_code_pos, start_code_len)) = find_annex_b_start_code(data, cursor) {
        found_start_code = true;
        let nal_start = start_code_pos + start_code_len;
        let nal_end = find_annex_b_start_code(data, nal_start)
            .map(|(next_start, _)| next_start)
            .unwrap_or(data.len());
        let nal = trim_annex_b_trailing_zeroes(&data[nal_start..nal_end]);
        if !nal.is_empty() && matches_nal(nal) {
            return Some(true);
        }
        cursor = nal_end;
    }
    found_start_code.then_some(false)
}

fn find_annex_b_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut index = from;
    while index + 3 <= data.len() {
        if data[index..].starts_with(&[0, 0, 1]) {
            return Some((index, 3));
        }
        if data[index..].starts_with(&[0, 0, 0, 1]) {
            return Some((index, 4));
        }
        index += 1;
    }
    None
}

fn access_unit_starts_with_annex_b_start_code(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

fn trim_annex_b_trailing_zeroes(nal: &[u8]) -> &[u8] {
    let mut end = nal.len();
    while end > 0 && nal[end - 1] == 0 {
        end -= 1;
    }
    &nal[..end]
}

fn access_unit_has_length_prefixed_nal(
    data: &[u8],
    length_size: usize,
    matches_nal: &mut impl FnMut(&[u8]) -> bool,
) -> Option<bool> {
    let mut cursor = 0;
    let mut found_nal = false;
    while cursor < data.len() {
        let len_end = cursor.checked_add(length_size)?;
        if len_end > data.len() {
            return None;
        }
        let nal_len = read_be_nal_len(&data[cursor..len_end])?;
        cursor = len_end;
        if nal_len == 0 {
            return None;
        }
        let nal_end = cursor.checked_add(nal_len)?;
        if nal_end > data.len() {
            return None;
        }
        found_nal = true;
        if matches_nal(&data[cursor..nal_end]) {
            return Some(true);
        }
        cursor = nal_end;
    }
    found_nal.then_some(false)
}

fn read_be_nal_len(bytes: &[u8]) -> Option<usize> {
    let mut len = 0usize;
    for byte in bytes {
        len = len.checked_shl(8)?.checked_add(usize::from(*byte))?;
    }
    Some(len)
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

    pub(super) fn ref_from(packet: &Self) -> std::result::Result<Self, String> {
        let clone = Self::new()?;
        let result = unsafe { ffi::av_packet_ref(clone.ptr, packet.ptr) };
        if result < 0 {
            return Err(format!("FFmpeg 复制 packet 失败：{}", ffmpeg_error(result)));
        }
        Ok(clone)
    }

    pub(super) fn props_from(packet: &Self) -> std::result::Result<Self, String> {
        let props = Self::new()?;
        let result = unsafe { ffi::av_packet_copy_props(props.ptr, packet.ptr) };
        if result < 0 {
            return Err(format!(
                "FFmpeg 复制 packet metadata 失败：{}",
                ffmpeg_error(result)
            ));
        }
        unsafe {
            (*props.ptr).stream_index = (*packet.ptr).stream_index;
        }
        Ok(props)
    }

    pub(super) fn from_data_and_props(
        data: &[u8],
        props: &Self,
    ) -> std::result::Result<Self, String> {
        let packet = Self::new()?;
        if !data.is_empty() {
            let size = c_int::try_from(data.len())
                .map_err(|_| "FFmpeg packet payload 过大".to_string())?;
            let result = unsafe { ffi::av_new_packet(packet.ptr, size) };
            if result < 0 {
                return Err(format!(
                    "FFmpeg 分配 packet payload 失败：{}",
                    ffmpeg_error(result)
                ));
            }
            let target = unsafe { (*packet.ptr).data };
            if target.is_null() {
                return Err("FFmpeg packet payload 为空".to_string());
            }
            unsafe { ptr::copy_nonoverlapping(data.as_ptr(), target, data.len()) };
        }
        let result = unsafe { ffi::av_packet_copy_props(packet.ptr, props.ptr) };
        if result < 0 {
            return Err(format!(
                "FFmpeg 恢复 packet metadata 失败：{}",
                ffmpeg_error(result)
            ));
        }
        unsafe {
            (*packet.ptr).stream_index = (*props.ptr).stream_index;
        }
        Ok(packet)
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

    pub(super) fn duration(&self) -> Option<i64> {
        let duration = unsafe { (*self.ptr).duration };
        (duration > 0).then_some(duration)
    }

    pub(super) fn is_key(&self) -> bool {
        unsafe { (*self.ptr).flags & ffi::AV_PKT_FLAG_KEY != 0 }
    }

    pub(super) fn size(&self) -> Option<u64> {
        let size = unsafe { (*self.ptr).size };
        (size > 0).then_some(size as u64)
    }

    pub(super) fn byte_len(&self) -> usize {
        self.size()
            .and_then(|size| usize::try_from(size).ok())
            .unwrap_or(0)
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

unsafe impl Send for AvPacket {}

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
    pub(super) fn new_for_frame(
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

#[cfg(test)]
mod tests {
    use ffmpeg_sys_next as ffi;

    use super::{
        AvPacket, audio_codec_requires_recovery_point, packet_is_audio_recovery_point,
        packet_is_video_recovery_point, packet_is_video_seek_point, video_error_recognition,
        vulkan_decode_codec_needs_single_thread,
    };

    fn packet_from_data(data: &[u8]) -> AvPacket {
        let props = AvPacket::new().expect("packet props allocate");
        AvPacket::from_data_and_props(data, &props).expect("packet data allocates")
    }

    #[test]
    fn h264_recovery_point_detects_annex_b_idr() {
        let packet = packet_from_data(&[0, 0, 1, 0x67, 0xaa, 0, 0, 0, 1, 0x65, 0xbb]);

        assert!(packet_is_video_recovery_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_H264
        ));
    }

    #[test]
    fn truehd_audio_recovery_point_detects_major_sync() {
        let packet = packet_from_data(&[0x01, 0x02, 0xf8, 0x72, 0x6f, 0xba, 0x03]);

        assert!(audio_codec_requires_recovery_point(
            ffi::AVCodecID::AV_CODEC_ID_TRUEHD
        ));
        assert!(packet_is_audio_recovery_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_TRUEHD
        ));
    }

    #[test]
    fn mlp_audio_recovery_point_detects_major_sync() {
        let packet = packet_from_data(&[0xf8, 0x72, 0x6f, 0xbb]);

        assert!(audio_codec_requires_recovery_point(
            ffi::AVCodecID::AV_CODEC_ID_MLP
        ));
        assert!(packet_is_audio_recovery_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_MLP
        ));
    }

    #[test]
    fn truehd_audio_recovery_point_rejects_non_sync_packet() {
        let packet = packet_from_data(&[0xf8, 0x72, 0x6f, 0xb9]);

        assert!(!packet_is_audio_recovery_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_TRUEHD
        ));
        assert!(!audio_codec_requires_recovery_point(
            ffi::AVCodecID::AV_CODEC_ID_AAC
        ));
    }

    #[test]
    fn h264_recovery_point_detects_length_prefixed_idr() {
        let packet = packet_from_data(&[0, 0, 0, 2, 0x65, 0xaa]);

        assert!(packet_is_video_recovery_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_H264
        ));
    }

    #[test]
    fn h264_recovery_point_rejects_key_packet_without_idr() {
        let mut packet = packet_from_data(&[0, 0, 0, 2, 0x41, 0xaa]);
        unsafe {
            (*packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
        }

        assert!(!packet_is_video_recovery_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_H264
        ));
    }

    #[test]
    fn hevc_recovery_point_detects_irap() {
        let packet = packet_from_data(&[0, 0, 0, 3, 0x26, 0x01, 0xaa]);

        assert!(packet_is_video_recovery_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_HEVC
        ));
    }

    #[test]
    fn video_error_recognition_keeps_hevc_decode_errors_non_fatal() {
        assert_eq!(
            video_error_recognition(ffi::AVCodecID::AV_CODEC_ID_H264),
            Some(ffi::AV_EF_BITSTREAM | ffi::AV_EF_BUFFER | ffi::AV_EF_EXPLODE)
        );
        assert_eq!(
            video_error_recognition(ffi::AVCodecID::AV_CODEC_ID_HEVC),
            Some(ffi::AV_EF_BITSTREAM | ffi::AV_EF_BUFFER)
        );
        assert_eq!(
            video_error_recognition(ffi::AVCodecID::AV_CODEC_ID_MPEG4),
            None
        );
    }

    #[test]
    fn vulkan_hevc_decode_always_uses_single_thread() {
        let newer_thread_safe_version = super::av_version_int(62, 28, 102);

        assert!(vulkan_decode_codec_needs_single_thread(
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            newer_thread_safe_version,
        ));
        assert!(!vulkan_decode_codec_needs_single_thread(
            ffi::AVCodecID::AV_CODEC_ID_H264,
            newer_thread_safe_version,
        ));
    }

    #[test]
    fn hevc_seek_point_requires_container_key_flag() {
        let mut packet = packet_from_data(&[0, 0, 0, 3, 0x26, 0x01, 0xaa]);

        assert!(packet_is_video_recovery_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_HEVC
        ));
        assert!(!packet_is_video_seek_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_HEVC
        ));

        unsafe {
            (*packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
        }
        assert!(packet_is_video_seek_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_HEVC
        ));
    }

    #[test]
    fn hevc_seek_point_rejects_cra_open_gop_keyframes() {
        let mut packet = packet_from_data(&[0, 0, 0, 3, 0x2a, 0x01, 0xaa]);
        unsafe {
            (*packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
        }

        assert!(packet_is_video_recovery_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_HEVC
        ));
        assert!(!packet_is_video_seek_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_HEVC
        ));
    }

    #[test]
    fn hevc_seek_point_rejects_mixed_access_unit_with_unsafe_vcl() {
        let mut packet = packet_from_data(&[
            0, 0, 0, 3, 0x26, 0x01, 0xaa, // IDR_W_RADL
            0, 0, 0, 3, 0x02, 0x01, 0xbb, // TRAIL_R
        ]);
        unsafe {
            (*packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
        }

        assert!(packet_is_video_recovery_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_HEVC
        ));
        assert!(!packet_is_video_seek_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_HEVC
        ));
    }

    #[test]
    fn hevc_recovery_point_detects_three_byte_length_prefixed_irap() {
        let packet = packet_from_data(&[0, 0, 3, 0x26, 0x01, 0xaa]);

        assert!(packet_is_video_recovery_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_HEVC
        ));
    }

    #[test]
    fn hevc_recovery_point_ignores_embedded_start_code_bytes_in_length_prefixed_payload() {
        let mut packet = packet_from_data(&[0, 0, 0, 7, 0x02, 0x01, 0, 0, 1, 0x26, 0x01]);
        unsafe {
            (*packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
        }

        assert!(!packet_is_video_recovery_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_HEVC
        ));
        assert!(!packet_is_video_seek_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_HEVC
        ));
    }

    #[test]
    fn hevc_seek_point_rejects_unknown_payload_layout() {
        let mut packet = packet_from_data(&[0xaa, 0xbb, 0xcc]);
        unsafe {
            (*packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
        }

        assert!(!packet_is_video_seek_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_HEVC
        ));
    }

    #[test]
    fn h264_recovery_point_falls_back_to_key_flag_for_unknown_payload_layout() {
        let mut packet = packet_from_data(&[0xaa, 0xbb, 0xcc]);
        unsafe {
            (*packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
        }

        assert!(packet_is_video_recovery_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_H264
        ));
    }

    #[test]
    fn generic_recovery_point_uses_packet_key_flag() {
        let mut packet = AvPacket::new().expect("packet allocates");
        assert!(!packet_is_video_recovery_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4
        ));

        unsafe {
            (*packet.as_mut_ptr()).flags = ffi::AV_PKT_FLAG_KEY;
        }
        assert!(packet_is_video_recovery_point(
            &packet,
            ffi::AVCodecID::AV_CODEC_ID_MPEG4
        ));
    }
}
