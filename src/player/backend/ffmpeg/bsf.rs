use std::{
    ffi::CString,
    os::raw::{c_char, c_int, c_void},
    ptr,
};

use ffmpeg_sys_next as ffi;

use super::{AvPacket, StreamInfo, ffmpeg_error};

#[repr(C)]
struct AvBitStreamFilter {
    _private: [u8; 0],
}

#[repr(C)]
struct AvBsfContext {
    av_class: *const c_void,
    filter: *const AvBitStreamFilter,
    priv_data: *mut c_void,
    par_in: *mut ffi::AVCodecParameters,
    par_out: *mut ffi::AVCodecParameters,
    time_base_in: ffi::AVRational,
    time_base_out: ffi::AVRational,
}

#[link(name = "avcodec")]
unsafe extern "C" {
    fn av_bsf_get_by_name(name: *const c_char) -> *const AvBitStreamFilter;
    fn av_bsf_alloc(filter: *const AvBitStreamFilter, ctx: *mut *mut AvBsfContext) -> c_int;
    fn av_bsf_init(ctx: *mut AvBsfContext) -> c_int;
    fn av_bsf_send_packet(ctx: *mut AvBsfContext, pkt: *mut ffi::AVPacket) -> c_int;
    fn av_bsf_receive_packet(ctx: *mut AvBsfContext, pkt: *mut ffi::AVPacket) -> c_int;
    fn av_bsf_flush(ctx: *mut AvBsfContext);
    fn av_bsf_free(ctx: *mut *mut AvBsfContext);
}

pub(super) struct PgsFrameMergeBitstreamFilter {
    ptr: *mut AvBsfContext,
}

impl PgsFrameMergeBitstreamFilter {
    pub(super) fn new(stream: StreamInfo) -> std::result::Result<Option<Self>, String> {
        if stream.codec_id != ffi::AVCodecID::AV_CODEC_ID_HDMV_PGS_SUBTITLE {
            return Ok(None);
        }

        let name = CString::new("pgs_frame_merge")
            .map_err(|_| "PGS bitstream filter 名称无效".to_string())?;
        let filter = unsafe { av_bsf_get_by_name(name.as_ptr()) };
        if filter.is_null() {
            tracing::warn!("FFmpeg pgs_frame_merge bitstream filter unavailable");
            return Ok(None);
        }

        let mut context = ptr::null_mut();
        let result = unsafe { av_bsf_alloc(filter, &mut context) };
        if result < 0 || context.is_null() {
            return Err(format!(
                "FFmpeg 创建 PGS bitstream filter 失败：{}",
                ffmpeg_error(result)
            ));
        }

        let codecpar = unsafe { (*stream.stream).codecpar };
        let result = unsafe { ffi::avcodec_parameters_copy((*context).par_in, codecpar) };
        if result < 0 {
            unsafe { av_bsf_free(&mut context) };
            return Err(format!(
                "FFmpeg 复制 PGS bitstream 参数失败：{}",
                ffmpeg_error(result)
            ));
        }
        unsafe { (*context).time_base_in = stream.time_base };

        let result = unsafe { av_bsf_init(context) };
        if result < 0 {
            unsafe { av_bsf_free(&mut context) };
            return Err(format!(
                "FFmpeg 初始化 PGS bitstream filter 失败：{}",
                ffmpeg_error(result)
            ));
        }

        tracing::debug!(
            stream_index = stream.index,
            "enabled FFmpeg pgs_frame_merge bitstream filter"
        );
        Ok(Some(Self { ptr: context }))
    }

    pub(super) fn send_packet(
        &mut self,
        packet: *mut ffi::AVPacket,
    ) -> std::result::Result<(), String> {
        let result = unsafe { av_bsf_send_packet(self.ptr, packet) };
        if result < 0 {
            return Err(format!(
                "FFmpeg 发送 PGS bitstream 包失败：{}",
                ffmpeg_error(result)
            ));
        }
        Ok(())
    }

    pub(super) fn receive_packet(
        &mut self,
        packet: &mut AvPacket,
    ) -> std::result::Result<bool, String> {
        let result = unsafe { av_bsf_receive_packet(self.ptr, packet.as_mut_ptr()) };
        if result == ffi::AVERROR(ffi::EAGAIN) || result == ffi::AVERROR_EOF {
            return Ok(false);
        }
        if result < 0 {
            return Err(format!(
                "FFmpeg 接收 PGS bitstream 包失败：{}",
                ffmpeg_error(result)
            ));
        }
        Ok(true)
    }

    pub(super) fn flush(&mut self) {
        unsafe { av_bsf_flush(self.ptr) };
    }
}

impl Drop for PgsFrameMergeBitstreamFilter {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { av_bsf_free(&mut self.ptr) };
        }
    }
}
