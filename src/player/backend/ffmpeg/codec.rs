use std::{
    env,
    ffi::CStr,
    mem,
    os::raw::{c_int, c_uint},
    ptr, slice,
    sync::Arc,
};

use crate::player::render_host::{FrameBufferPool, PooledBytes, RenderSize, VulkanDecodeDevice};
use ffmpeg_sys_next as ffi;

use super::audio::{audio_sample_len, frame_sample_format, zeroed_channel_layout};
use super::subtitle::{DecodedSubtitleCue, decoded_subtitle_cues};
use super::{
    FALLBACK_AUDIO_OUTPUT_CHANNELS, HardwareDecodeMode, StreamInfo, VideoHwDecodeContext,
    ffmpeg_error, frame_size, video_frame_len,
};

const VULKAN_THREAD_SAFE_LIBAVCODEC_VERSION: c_uint = av_version_int(62, 11, 100);
const TINY_VULKAN_DECODE_THREADS_ENV: &str = "TINY_VULKAN_DECODE_THREADS";
const DEFAULT_VULKAN_DECODE_THREADS: c_int = 4;

pub(super) struct Decoder {
    ptr: *mut ffi::AVCodecContext,
    pub(super) stream_index: c_int,
    pub(super) time_base: ffi::AVRational,
    video_hw: Option<VideoHwDecodeContext>,
    hardware_decode_mode: HardwareDecodeMode,
    hw_format_selection: Option<Box<VideoHwFormatSelection>>,
}

// Decoder is moved into a dedicated worker thread and then accessed only from that thread.
unsafe impl Send for Decoder {}

#[path = "codec/decoder_open.rs"]
mod decoder_open;
#[path = "codec/decoder_runtime.rs"]
mod decoder_runtime;

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
    _codec_id: ffi::AVCodecID,
    avcodec_version: c_uint,
) -> bool {
    avcodec_version < VULKAN_THREAD_SAFE_LIBAVCODEC_VERSION
}

fn parse_vulkan_decode_thread_count(value: &str) -> Option<c_int> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" => Some(1),
        "4" => Some(4),
        "" | "0" | "auto" => Some(0),
        _ => None,
    }
}

fn configured_vulkan_decode_thread_count() -> c_int {
    let Ok(value) = env::var(TINY_VULKAN_DECODE_THREADS_ENV) else {
        return DEFAULT_VULKAN_DECODE_THREADS;
    };
    parse_vulkan_decode_thread_count(&value).unwrap_or_else(|| {
        tracing::warn!(
            value,
            default_threads = DEFAULT_VULKAN_DECODE_THREADS,
            "ignored invalid TINY_VULKAN_DECODE_THREADS; expected 1, 4, or auto"
        );
        DEFAULT_VULKAN_DECODE_THREADS
    })
}

fn video_error_recognition(codec_id: ffi::AVCodecID) -> Option<c_int> {
    match codec_id {
        ffi::AVCodecID::AV_CODEC_ID_H264 => {
            Some(ffi::AV_EF_BITSTREAM | ffi::AV_EF_BUFFER | ffi::AV_EF_EXPLODE)
        }
        // FFmpeg otherwise logs a missing HEVC reference, swallows
        // AVERROR_INVALIDDATA at the NAL boundary, and reports the packet as
        // consumed. That hides a broken RPS from tiny until the next IDR makes
        // the multi-second output hole observable. EXPLODE preserves FFmpeg's
        // normal bitstream checks while surfacing this error immediately to
        // the existing flush-and-realign recovery path.
        ffi::AVCodecID::AV_CODEC_ID_HEVC => Some(ffi::AV_EF_EXPLODE),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum VideoRecoveryPointKind {
    #[default]
    None,
    Cra,
    Idr,
    Bla,
    Keyframe,
}

#[path = "codec/recovery_points.rs"]
mod recovery_points;

pub(super) fn packet_video_recovery_point_kind(
    packet: &AvPacket,
    codec_id: ffi::AVCodecID,
) -> VideoRecoveryPointKind {
    match codec_id {
        ffi::AVCodecID::AV_CODEC_ID_H264 => packet
            .data()
            .map(|data| {
                h264_access_unit_recovery_state(data)
                    .map(|recovery| {
                        if recovery {
                            VideoRecoveryPointKind::Idr
                        } else {
                            VideoRecoveryPointKind::None
                        }
                    })
                    .unwrap_or_else(|| {
                        if packet.is_key() {
                            VideoRecoveryPointKind::Keyframe
                        } else {
                            VideoRecoveryPointKind::None
                        }
                    })
            })
            .unwrap_or_else(|| {
                if packet.is_key() {
                    VideoRecoveryPointKind::Keyframe
                } else {
                    VideoRecoveryPointKind::None
                }
            }),
        ffi::AVCodecID::AV_CODEC_ID_HEVC => packet
            .data()
            .map(|data| {
                hevc_access_unit_recovery_kind(data).unwrap_or_else(|| {
                    if packet.is_key() {
                        VideoRecoveryPointKind::Keyframe
                    } else {
                        VideoRecoveryPointKind::None
                    }
                })
            })
            .unwrap_or_else(|| {
                if packet.is_key() {
                    VideoRecoveryPointKind::Keyframe
                } else {
                    VideoRecoveryPointKind::None
                }
            }),
        _ if packet.is_key() => VideoRecoveryPointKind::Keyframe,
        _ => VideoRecoveryPointKind::None,
    }
}

pub(super) fn packet_is_video_recovery_point(packet: &AvPacket, codec_id: ffi::AVCodecID) -> bool {
    packet_video_recovery_point_kind(packet, codec_id).is_recovery_point()
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

fn hevc_access_unit_recovery_kind(data: &[u8]) -> Option<VideoRecoveryPointKind> {
    let mut recovery_kind = VideoRecoveryPointKind::None;
    access_unit_nal_recovery_state(data, |nal| {
        let Some(header) = nal.first() else {
            return false;
        };
        recovery_kind = match hevc_nal_type(*header) {
            HEVC_NAL_BLA_W_LP | HEVC_NAL_BLA_W_RADL | HEVC_NAL_BLA_N_LP => {
                VideoRecoveryPointKind::Bla
            }
            HEVC_NAL_IDR_W_RADL | HEVC_NAL_IDR_N_LP => VideoRecoveryPointKind::Idr,
            HEVC_NAL_CRA => VideoRecoveryPointKind::Cra,
            _ => VideoRecoveryPointKind::None,
        };
        recovery_kind.is_recovery_point()
    })
    .map(|_| recovery_kind)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AvPacketStorageKind {
    Memory,
    Disk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AvPacketReadDiagnostic {
    pub(super) read_sequence: u64,
    pub(super) cache_generation: u64,
    pub(super) read_range_id: u64,
    pub(super) packet_id: u64,
    pub(super) stream_offset: usize,
    pub(super) storage: AvPacketStorageKind,
    pub(super) read_index_before: usize,
    pub(super) read_index_after: usize,
    pub(super) reader_head_before: Option<u64>,
    pub(super) reader_head_after: Option<u64>,
    pub(super) previous_read_packet_id: Option<u64>,
    pub(super) previous_read_generation: Option<u64>,
    pub(super) previous_expected_next_packet_id: Option<u64>,
    pub(super) sequence_contiguous: Option<bool>,
    pub(super) packet_start_nsecs: Option<u64>,
    pub(super) packet_end_nsecs: Option<u64>,
    pub(super) timeline_anchor: bool,
    pub(super) recovery_point: bool,
    pub(super) recovery_kind: VideoRecoveryPointKind,
    pub(super) safe_seek_point: bool,
}

pub(super) struct AvPacket {
    ptr: *mut ffi::AVPacket,
    read_diagnostic: Option<Arc<AvPacketReadDiagnostic>>,
}

#[path = "codec/packet.rs"]
mod packet;

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

#[path = "codec/frame.rs"]
mod frame;

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

#[path = "codec/video_scaler.rs"]
mod video_scaler;

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

#[path = "codec/audio_resampler.rs"]
mod audio_resampler;

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
        AvPacket, AvPacketReadDiagnostic, AvPacketStorageKind, VideoRecoveryPointKind,
        audio_codec_requires_recovery_point, packet_is_audio_recovery_point,
        packet_is_video_recovery_point, packet_is_video_seek_point,
        packet_video_recovery_point_kind, parse_vulkan_decode_thread_count,
        video_error_recognition, vulkan_decode_codec_needs_single_thread,
    };

    fn packet_from_data(data: &[u8]) -> AvPacket {
        let props = AvPacket::new().expect("packet props allocate");
        AvPacket::from_data_and_props(data, &props).expect("packet data allocates")
    }

    #[test]
    fn packet_read_diagnostic_survives_packet_clones_and_payload_rebuilds() {
        let mut packet = packet_from_data(&[1, 2, 3, 4]);
        let diagnostic = AvPacketReadDiagnostic {
            read_sequence: 7,
            cache_generation: 3,
            read_range_id: 2,
            packet_id: 41,
            stream_offset: 1,
            storage: AvPacketStorageKind::Disk,
            read_index_before: 8,
            read_index_after: 9,
            reader_head_before: Some(41),
            reader_head_after: Some(44),
            previous_read_packet_id: Some(38),
            previous_read_generation: Some(3),
            previous_expected_next_packet_id: Some(41),
            sequence_contiguous: Some(true),
            packet_start_nsecs: Some(1_000_000_000),
            packet_end_nsecs: Some(1_040_000_000),
            timeline_anchor: true,
            recovery_point: false,
            recovery_kind: VideoRecoveryPointKind::None,
            safe_seek_point: false,
        };
        packet.set_read_diagnostic(diagnostic);

        let cloned = AvPacket::ref_from(&packet).expect("packet clone succeeds");
        let props = AvPacket::props_from(&cloned).expect("packet props clone succeeds");
        let rebuilt =
            AvPacket::from_data_and_props(&[5, 6], &props).expect("packet rebuild succeeds");

        assert_eq!(cloned.read_diagnostic(), Some(diagnostic));
        assert_eq!(props.read_diagnostic(), Some(diagnostic));
        assert_eq!(rebuilt.read_diagnostic(), Some(diagnostic));
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
    fn hevc_recovery_point_classifies_cra_idr_and_bla() {
        for (header, expected) in [
            (0x2a, VideoRecoveryPointKind::Cra),
            (0x26, VideoRecoveryPointKind::Idr),
            (0x20, VideoRecoveryPointKind::Bla),
        ] {
            let packet = packet_from_data(&[0, 0, 0, 3, header, 0x01, 0xaa]);
            assert_eq!(
                packet_video_recovery_point_kind(&packet, ffi::AVCodecID::AV_CODEC_ID_HEVC),
                expected
            );
        }
    }

    #[test]
    fn video_error_recognition_surfaces_hevc_rps_failures() {
        assert_eq!(
            video_error_recognition(ffi::AVCodecID::AV_CODEC_ID_H264),
            Some(ffi::AV_EF_BITSTREAM | ffi::AV_EF_BUFFER | ffi::AV_EF_EXPLODE)
        );
        assert_eq!(
            video_error_recognition(ffi::AVCodecID::AV_CODEC_ID_HEVC),
            Some(ffi::AV_EF_EXPLODE)
        );
        assert_eq!(
            video_error_recognition(ffi::AVCodecID::AV_CODEC_ID_MPEG4),
            None
        );
    }

    #[test]
    fn vulkan_decode_uses_single_thread_only_before_ffmpeg_thread_safety_fix() {
        let older_unsafe_version = super::av_version_int(62, 11, 99);
        let newer_thread_safe_version = super::av_version_int(62, 28, 102);

        assert!(vulkan_decode_codec_needs_single_thread(
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            older_unsafe_version,
        ));
        assert!(!vulkan_decode_codec_needs_single_thread(
            ffi::AVCodecID::AV_CODEC_ID_HEVC,
            newer_thread_safe_version,
        ));
        assert!(!vulkan_decode_codec_needs_single_thread(
            ffi::AVCodecID::AV_CODEC_ID_H264,
            newer_thread_safe_version,
        ));
    }

    #[test]
    fn vulkan_decode_thread_ab_values_parse_as_one_four_or_auto() {
        assert_eq!(parse_vulkan_decode_thread_count("1"), Some(1));
        assert_eq!(parse_vulkan_decode_thread_count("4"), Some(4));
        assert_eq!(parse_vulkan_decode_thread_count("auto"), Some(0));
        assert_eq!(parse_vulkan_decode_thread_count("0"), Some(0));
        assert_eq!(parse_vulkan_decode_thread_count("16"), None);
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
