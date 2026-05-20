use super::*;
use crate::player::render_host::{RawVideoFrame, RawVideoPlane, RawVideoPlanes};

#[test]
fn swap_red_blue_channels_swaps_red_and_blue_channels() {
    let mut pixels = vec![1, 2, 3, 4, 5, 6, 7, 8];

    swap_red_blue_channels(&mut pixels);

    assert_eq!(pixels, [3, 2, 1, 4, 7, 6, 5, 8]);
}

#[test]
fn vulkan_import_queues_use_dedicated_queues_when_available() {
    let queues = VulkanDecodeQueues {
        graphics: VulkanDecodeQueue { index: 0, count: 1 },
        compute: Some(VulkanDecodeQueue { index: 1, count: 2 }),
        transfer: Some(VulkanDecodeQueue { index: 2, count: 1 }),
    };

    let import = vulkan_import_queues(queues);

    assert_eq!(import.graphics, queues.graphics);
    assert_eq!(import.compute, queues.compute.unwrap());
    assert_eq!(import.transfer, queues.transfer.unwrap());
    assert!(!import.no_compute);
}

#[test]
fn vulkan_import_queues_fall_back_to_graphics_without_compute() {
    let queues = VulkanDecodeQueues {
        graphics: VulkanDecodeQueue { index: 3, count: 1 },
        compute: None,
        transfer: None,
    };

    let import = vulkan_import_queues(queues);

    assert_eq!(import.graphics, queues.graphics);
    assert_eq!(import.compute, queues.graphics);
    assert_eq!(import.transfer, queues.graphics);
    assert!(import.no_compute);
}

#[test]
fn dolby_vision_profile5_uses_raw_frame_range_input() {
    let full = unsafe {
        source_color_repr(
            RawVideoFormat::P010Le,
            FrameColor::DolbyVisionProfile5,
            RawVideoRange::Full,
        )
    };
    let limited = unsafe {
        source_color_repr(
            RawVideoFormat::P010Le,
            FrameColor::DolbyVisionProfile5,
            RawVideoRange::Limited,
        )
    };
    let unknown = unsafe {
        source_color_repr(
            RawVideoFormat::P010Le,
            FrameColor::DolbyVisionProfile5,
            RawVideoRange::Unknown,
        )
    };

    assert_eq!(full.levels, ffi::pl_color_levels_PL_COLOR_LEVELS_FULL);
    assert_eq!(limited.levels, ffi::pl_color_levels_PL_COLOR_LEVELS_LIMITED);
    assert_eq!(unknown.levels, ffi::pl_color_levels_PL_COLOR_LEVELS_UNKNOWN);
    assert_eq!(full.sys, ffi::pl_color_system_PL_COLOR_SYSTEM_DOLBYVISION);
}

#[test]
fn dolby_vision_color_matrices_use_distinct_denominators() {
    assert_eq!(dovi_matrix_coefficient(8192, 13), 1.0);
    assert_eq!(dovi_matrix_coefficient(16384, 14), 1.0);
    assert_eq!(dovi_matrix_coefficient(8192, 14), 0.5);
}

#[test]
fn dolby_vision_float_coefficients_use_ieee_bits() {
    assert_eq!(
        dovi_coefficient(1, 1.0, None, Some(1.25f32.to_bits().into())),
        1.25
    );
    assert_eq!(
        dovi_coefficient(1, 1.0, None, Some((-0.5f32).to_bits().into())),
        -0.5
    );
}

#[test]
fn dolby_vision_fixed_coefficients_match_ffmpeg_fixed_point() {
    assert_eq!(dovi_coefficient(0, 0.25, Some(1), Some(3)), 1.75);
    assert_eq!(dovi_coefficient(0, 0.25, Some(-1), Some(1)), -0.75);
    assert_eq!(dovi_coefficient(0, 0.25, Some(-2), Some(3)), -1.25);
}

#[test]
fn dolby_vision_mapping_accumulates_rpu_pivot_deltas() {
    let mut rpu = DoviRpu::default();
    rpu.header = dolby_vision::rpu::rpu_data_header::RpuDataHeader {
        bl_bit_depth_minus8: 2,
        coefficient_data_type: 0,
        coefficient_log2_denom: 23,
        ..Default::default()
    };
    let mut mapping = RpuDataMapping::default();
    mapping.curves[0].pivots = vec![100, 200, 300];
    mapping.curves[0].mapping_idc = DoviMappingMethod::Polynomial;

    let dovi = map_dovi_metadata(&rpu, &mapping, &profile5_default_dovi_color()).unwrap();

    assert_approx_eq(dovi.comp[0].pivots[0], 100.0 / 1023.0);
    assert_approx_eq(dovi.comp[0].pivots[1], 300.0 / 1023.0);
    assert_approx_eq(dovi.comp[0].pivots[2], 600.0 / 1023.0);
}

#[test]
fn dolby_vision_hdr_metadata_preserves_default_when_source_luminance_is_unknown() {
    let mut color = unsafe { source_color_space(FrameColor::DolbyVisionProfile5) };
    let min_luma = color.hdr.min_luma;
    let max_luma = color.hdr.max_luma;
    let metadata = DoviRenderMetadata {
        placebo: unsafe { mem::zeroed() },
        cache_key: Vec::new(),
        source_min_pq: 0,
        source_max_pq: 0,
    };

    apply_dovi_source_luminance_metadata(&mut color, &metadata);

    assert_eq!(color.hdr.min_luma, min_luma);
    assert_eq!(color.hdr.max_luma, max_luma);
}

#[test]
fn dolby_vision_profile5_requires_rpu_metadata() {
    let frame = RawVideoFrame {
        format: RawVideoFormat::P010Le,
        color: FrameColor::DolbyVisionProfile5,
        range: RawVideoRange::Limited,
        chroma_site: RawVideoChromaSite::Left,
        metadata: None,
        planes: RawVideoPlanes::Owned(Vec::new()),
    };

    let mut cache = DoviMetadataCache::default();
    let error = match cache.prepare_raw_video(&frame) {
        Ok(_) => panic!("Dolby Vision Profile 5 without RPU should fail"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("缺少 RPU 元数据"));
}

#[test]
fn dolby_vision_cache_reuses_uncompressed_color_metadata() {
    let mut cache = DoviMetadataCache::default();
    let color = VdrDmData {
        compressed: false,
        ycc_to_rgb_coef0: 8192,
        source_min_pq: 7,
        source_max_pq: 3079,
        ..Default::default()
    };

    let first = cache.resolve_color(8, Some(color)).unwrap();
    let reused = cache
        .resolve_color(
            8,
            Some(VdrDmData {
                compressed: true,
                ..Default::default()
            }),
        )
        .unwrap();

    assert_eq!(first.ycc_to_rgb_coef0, 8192);
    assert_eq!(reused.ycc_to_rgb_coef0, 8192);
    assert_eq!(reused.source_min_pq, 7);
    assert_eq!(reused.source_max_pq, 3079);
}

#[test]
fn dolby_vision_profile5_prefers_stream_color_metadata() {
    let mut cache = DoviMetadataCache::default();

    let color = cache
        .resolve_color(
            5,
            Some(VdrDmData {
                compressed: false,
                ycc_to_rgb_coef0: 1,
                source_min_pq: 7,
                source_max_pq: 3079,
                ..Default::default()
            }),
        )
        .unwrap();

    assert_eq!(color.ycc_to_rgb_coef0, 1);
    assert_eq!(color.source_min_pq, 7);
    assert_eq!(color.source_max_pq, 3079);
}

#[test]
fn dolby_vision_profile5_detects_dovi_tool_default_color_metadata() {
    let color = VdrDmData {
        compressed: false,
        ycc_to_rgb_coef0: 8192,
        ycc_to_rgb_coef1: 799,
        ycc_to_rgb_coef2: 1681,
        ycc_to_rgb_coef3: 8192,
        ycc_to_rgb_coef4: -933,
        ycc_to_rgb_coef5: 1091,
        ycc_to_rgb_coef6: 8192,
        ycc_to_rgb_coef7: 267,
        ycc_to_rgb_coef8: -5545,
        ycc_to_rgb_offset0: 0,
        ycc_to_rgb_offset1: 134_217_728,
        ycc_to_rgb_offset2: 134_217_728,
        rgb_to_lms_coef0: 17081,
        rgb_to_lms_coef1: -349,
        rgb_to_lms_coef2: -349,
        rgb_to_lms_coef3: -349,
        rgb_to_lms_coef4: 17081,
        rgb_to_lms_coef5: -349,
        rgb_to_lms_coef6: -349,
        rgb_to_lms_coef7: -349,
        rgb_to_lms_coef8: 17081,
        source_min_pq: 7,
        source_max_pq: 3079,
        ..Default::default()
    };

    assert!(is_dovi_tool_profile5_default_color(&color));
}

#[test]
fn dolby_vision_profile5_uses_cached_color_for_compressed_metadata() {
    let mut cache = DoviMetadataCache::default();
    let first = cache
        .resolve_color(
            5,
            Some(VdrDmData {
                compressed: false,
                ycc_to_rgb_coef0: 1,
                source_min_pq: 7,
                source_max_pq: 3079,
                ..Default::default()
            }),
        )
        .unwrap();
    let reused = cache
        .resolve_color(
            5,
            Some(VdrDmData {
                compressed: true,
                ..Default::default()
            }),
        )
        .unwrap();

    assert_eq!(first.ycc_to_rgb_coef0, 1);
    assert_eq!(reused.ycc_to_rgb_coef0, 1);
    assert_eq!(reused.source_max_pq, 3079);
}

#[test]
fn dolby_vision_profile5_uses_profile_default_color_for_initial_compressed_metadata() {
    let mut cache = DoviMetadataCache::default();

    let color = cache
        .resolve_color(
            5,
            Some(VdrDmData {
                compressed: true,
                ..Default::default()
            }),
        )
        .unwrap();

    assert!(is_dovi_tool_profile5_default_color(&color));
    assert_eq!(color.signal_color_space, 2);
    assert_eq!(color.signal_bit_depth, 12);
    assert_eq!(color.signal_full_range_flag, 1);
}

#[test]
fn dolby_vision_cache_rejects_unknown_compressed_color_without_previous_metadata() {
    let mut cache = DoviMetadataCache::default();

    let error = cache
        .resolve_color(
            8,
            Some(VdrDmData {
                compressed: true,
                ..Default::default()
            }),
        )
        .unwrap_err();

    assert!(error.to_string().contains("可复用的 color metadata"));
}

#[test]
fn libplacebo_tone_maps_p010_when_enabled() {
    if std::env::var("TINY_TEST_LIBPLACEBO").as_deref() != Ok("1") {
        return;
    }

    let mut tone_mapper = LibplaceboToneMapper::new().unwrap();
    let y = p010_code(940);
    let uv = p010_code(512);
    let frame = p010_frame(FrameColor::Hdr10Bt2020, [y; 4], [uv, uv]);

    let pixels = tone_mapper
        .tone_map_to_bgra8(
            &frame,
            RenderSize {
                width: 2,
                height: 2,
            },
            RenderSize {
                width: 2,
                height: 2,
            },
        )
        .unwrap();

    assert_eq!(pixels.len(), 16);
    assert!(pixels.iter().any(|value| *value > 0));
}

#[test]
fn libplacebo_tone_maps_hdr10_p010_red_as_red_when_enabled() {
    if std::env::var("TINY_TEST_LIBPLACEBO").as_deref() != Ok("1") {
        return;
    }

    let mut tone_mapper = LibplaceboToneMapper::new().unwrap();
    let frame = p010_frame(
        FrameColor::Hdr10Bt2020,
        [p010_code(294); 4],
        [p010_code(449), p010_code(736)],
    );

    let pixels = tone_mapper
        .tone_map_to_bgra8(
            &frame,
            RenderSize {
                width: 2,
                height: 2,
            },
            RenderSize {
                width: 2,
                height: 2,
            },
        )
        .unwrap();

    assert_bgra_red_dominant(&pixels);
}

#[test]
fn libplacebo_tone_maps_hdr10_i42010_red_as_red_when_enabled() {
    if std::env::var("TINY_TEST_LIBPLACEBO").as_deref() != Ok("1") {
        return;
    }

    let mut tone_mapper = LibplaceboToneMapper::new().unwrap();
    let frame = i42010_frame(FrameColor::Hdr10Bt2020, [294; 4], [449], [736]);

    let pixels = tone_mapper
        .tone_map_to_bgra8(
            &frame,
            RenderSize {
                width: 2,
                height: 2,
            },
            RenderSize {
                width: 2,
                height: 2,
            },
        )
        .unwrap();

    assert_bgra_red_dominant(&pixels);
}

fn assert_bgra_red_dominant(pixels: &[u8]) {
    assert!(
        pixels[2] > pixels[0],
        "expected BGRA red > blue, got {:?}",
        &pixels[..4]
    );
}

fn assert_approx_eq(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() < 0.000_001,
        "expected {expected}, got {actual}"
    );
}

fn p010_frame(color: FrameColor, y: [u16; 4], uv: [u16; 2]) -> RawVideoFrame {
    RawVideoFrame {
        format: RawVideoFormat::P010Le,
        color,
        range: RawVideoRange::Limited,
        chroma_site: RawVideoChromaSite::Left,
        metadata: None,
        planes: RawVideoPlanes::Owned(vec![
            RawVideoPlane {
                data: y
                    .into_iter()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>()
                    .into(),
                stride: 4,
            },
            RawVideoPlane {
                data: uv
                    .into_iter()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>()
                    .into(),
                stride: 4,
            },
        ]),
    }
}

fn i42010_frame(color: FrameColor, y: [u16; 4], u: [u16; 1], v: [u16; 1]) -> RawVideoFrame {
    RawVideoFrame {
        format: RawVideoFormat::I42010Le,
        color,
        range: RawVideoRange::Limited,
        chroma_site: RawVideoChromaSite::Left,
        metadata: None,
        planes: RawVideoPlanes::Owned(vec![
            RawVideoPlane {
                data: y
                    .into_iter()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>()
                    .into(),
                stride: 4,
            },
            RawVideoPlane {
                data: u
                    .into_iter()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>()
                    .into(),
                stride: 2,
            },
            RawVideoPlane {
                data: v
                    .into_iter()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>()
                    .into(),
                stride: 2,
            },
        ]),
    }
}

fn p010_code(value: u16) -> u16 {
    value << 6
}
