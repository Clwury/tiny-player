use std::{mem, slice};

use super::*;
use crate::{
    backend::ffmpeg::{VideoFrameConvertContext, VideoFrameConverter},
    render_host::{
        FrameBufferPool, FrameColor, FramePixels, RawVideoChromaSite, RawVideoFormat,
        RawVideoPlanes, RawVideoRange, RenderSize,
    },
};

const METADATA: &[u8] = b"vaapi-copyback-metadata";

fn hardware_frame(format: ffi::AVPixelFormat) -> AvFrame {
    let mut frame = AvFrame::new().unwrap();
    unsafe {
        let frame = &mut *frame.as_mut_ptr();
        frame.format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32;
        frame.width = 60;
        frame.height = 48;
        frame.pts = 1_234;
        frame.best_effort_timestamp = 1_234;
        frame.duration = 40;
        frame.flags = ffi::AV_FRAME_FLAG_KEY;
        frame.color_range = ffi::AVColorRange::AVCOL_RANGE_MPEG;
        frame.color_primaries = ffi::AVColorPrimaries::AVCOL_PRI_BT2020;
        frame.colorspace = ffi::AVColorSpace::AVCOL_SPC_BT2020_NCL;
        frame.color_trc = ffi::AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084;
        frame.chroma_location = ffi::AVChromaLocation::AVCHROMA_LOC_LEFT;
        // A plain buffer describes the pool without constructing a VA display.
        // The transfer gateway below supplies pixels without touching a driver.
        frame.hw_frames_ctx = ffi::av_buffer_allocz(mem::size_of::<ffi::AVHWFramesContext>());
        assert!(!frame.hw_frames_ctx.is_null());
        let frames = &mut *((*frame.hw_frames_ctx).data as *mut ffi::AVHWFramesContext);
        frames.format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
        frames.sw_format = format;
        frames.width = 64;
        frames.height = 64;
        let side_data = ffi::av_frame_new_side_data(
            frame,
            ffi::AVFrameSideDataType::AV_FRAME_DATA_SEI_UNREGISTERED,
            METADATA.len(),
        );
        assert!(!side_data.is_null());
        ptr::copy_nonoverlapping(METADATA.as_ptr(), (*side_data).data, METADATA.len());
    }
    frame
}

#[test]
fn vaapi_copyback_preserves_bit_depth_hdr_properties_and_visible_crop() {
    for (format, expected) in [
        (ffi::AVPixelFormat::AV_PIX_FMT_NV12, RawVideoFormat::Nv12),
        (
            ffi::AVPixelFormat::AV_PIX_FMT_P010LE,
            RawVideoFormat::P010Le,
        ),
    ] {
        let mut hardware = hardware_frame(format);
        let mut downloaded =
            download_frame_with(hardware.as_mut_ptr(), |destination, source| unsafe {
                assert_eq!((*destination).format, format as i32);
                (*destination).width = 64;
                (*destination).height = 64;
                let result = ffi::av_frame_get_buffer(destination, 32);
                assert!(result >= 0);
                for plane in 0..expected.plane_count() {
                    let layout = expected
                        .plane_layout(
                            RenderSize {
                                width: 64,
                                height: 64,
                            },
                            plane,
                        )
                        .unwrap();
                    for row in 0..layout.height as usize {
                        let bytes = slice::from_raw_parts_mut(
                            (*destination).data[plane]
                                .add(row * (*destination).linesize[plane] as usize),
                            layout.row_len,
                        );
                        for sample in bytes.chunks_exact_mut(expected.component_size() as usize / 8)
                        {
                            match expected {
                                RawVideoFormat::Nv12 => sample[0] = 128,
                                // Nonzero low 10-bit precision catches an accidental
                                // conversion through an 8-bit intermediate.
                                RawVideoFormat::P010Le => {
                                    sample.copy_from_slice(&(513_u16 << 6).to_le_bytes())
                                }
                                _ => unreachable!(),
                            }
                        }
                    }
                }
                (*destination).width = (*source).width;
                (*destination).height = (*source).height;
                0
            })
            .unwrap();
        let ptr = downloaded.as_mut_ptr();
        unsafe {
            assert_eq!((*ptr).pts, 1_234);
            assert_eq!((*ptr).best_effort_timestamp, 1_234);
            assert_eq!((*ptr).duration, 40);
            assert!((*ptr).hw_frames_ctx.is_null());
            let side_data = ffi::av_frame_get_side_data(
                ptr,
                ffi::AVFrameSideDataType::AV_FRAME_DATA_SEI_UNREGISTERED,
            );
            assert!(!side_data.is_null());
            assert_eq!(
                slice::from_raw_parts((*side_data).data, (*side_data).size),
                METADATA
            );
        }
        let mut converter = VideoFrameConverter::new(FrameBufferPool::default());
        let converted = converter
            .convert_with_context(
                &VideoFrameConvertContext::new_for_test(RenderSize {
                    width: 60,
                    height: 48,
                }),
                ptr,
                None,
            )
            .unwrap();
        assert!(converted.key_frame);
        assert_eq!(
            converted.size,
            RenderSize {
                width: 60,
                height: 48
            }
        );
        let FramePixels::RawVideo(raw) = converted.pixels else {
            panic!("copyback must retain YUV pixels")
        };
        assert_eq!(raw.format, expected);
        assert_eq!(raw.color, FrameColor::Hdr10Bt2020);
        assert_eq!(raw.range, RawVideoRange::Limited);
        assert_eq!(raw.chroma_site, RawVideoChromaSite::Left);
        let RawVideoPlanes::Owned(planes) = raw.planes;
        for (index, plane) in planes.iter().enumerate() {
            let layout = expected.plane_layout(converted.size, index).unwrap();
            assert_eq!(plane.stride, layout.row_len);
            assert_eq!(plane.data.len(), layout.row_len * layout.height as usize);
            match expected {
                RawVideoFormat::Nv12 => assert!(plane.data.iter().all(|value| *value == 128)),
                RawVideoFormat::P010Le => assert!(
                    plane
                        .data
                        .chunks_exact(2)
                        .all(|sample| u16::from_le_bytes([sample[0], sample[1]]) == 513 << 6)
                ),
                _ => unreachable!(),
            }
        }
    }
}

#[test]
fn vaapi_copyback_reports_transfer_failure_without_software_scaling() {
    let mut frame = hardware_frame(ffi::AVPixelFormat::AV_PIX_FMT_P010LE);
    let error = download_frame_with(frame.as_mut_ptr(), |_, _| ffi::AVERROR(ffi::EIO))
        .err()
        .unwrap();
    assert!(error.contains("下载 VA-API"), "{error}");
    let mut unsupported = hardware_frame(ffi::AVPixelFormat::AV_PIX_FMT_YUV444P16LE);
    let error = download_frame_with(unsupported.as_mut_ptr(), |_, _| {
        panic!("unsupported depth must not be downloaded as NV12")
    })
    .err()
    .unwrap();
    assert!(error.contains("不支持像素格式"), "{error}");
    let mut invalid = AvFrame::new().unwrap();
    unsafe { (*invalid.as_mut_ptr()).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32 };
    let error = download_vaapi_frame(invalid.as_mut_ptr()).err().unwrap();
    assert!(error.contains("hw_frames_ctx"), "{error}");
}

#[test]
fn vaapi_decoder_selection_bypasses_software_only_av1_decoder_when_available() {
    let native = unsafe { ffi::avcodec_find_decoder_by_name(c"av1".as_ptr()) };
    let software = unsafe { ffi::avcodec_find_decoder_by_name(c"libdav1d".as_ptr()) };
    if native.is_null() || software.is_null() || !supports_vaapi(native) {
        return;
    }
    assert!(!supports_vaapi(software));
    let selected = find_vaapi_decoder(software).unwrap();
    assert!(supports_vaapi(selected));
    assert_eq!(unsafe { (*selected).id }, ffi::AVCodecID::AV_CODEC_ID_AV1);
}

#[test]
fn vaapi_decoder_selection_rejects_unsupported_codec_without_falling_back() {
    let decoder = unsafe { ffi::avcodec_find_decoder(ffi::AVCodecID::AV_CODEC_ID_RAWVIDEO) };
    assert!(find_vaapi_decoder(decoder).is_err());
}
