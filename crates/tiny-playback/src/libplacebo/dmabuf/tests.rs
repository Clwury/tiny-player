use super::*;

#[test]
fn va_driver_chroma_fourcc_aliases_use_the_same_component_layout() {
    for (format, aliases, expected) in [
        (RawVideoFormat::Nv12, [*b"GR88", *b"RG88"], c"rg8"),
        (RawVideoFormat::P010Le, [*b"GR32", *b"RG32"], c"rg16"),
    ] {
        for alias in aliases {
            assert_eq!(
                plane_format_name(format, 1, u32::from_le_bytes(alias)).unwrap(),
                expected
            );
        }
    }
    assert!(plane_format_name(RawVideoFormat::Nv12, 1, u32::from_le_bytes(*b"R8  ")).is_err());
}

fn descriptor() -> ffmpeg_vulkan::AVDRMFrameDescriptor {
    let mut descriptor: ffmpeg_vulkan::AVDRMFrameDescriptor = unsafe { mem::zeroed() };
    descriptor.nb_objects = 1;
    descriptor.objects[0].fd = 5;
    descriptor.objects[0].size = 4096;
    descriptor.nb_layers = 2;
    for layer in &mut descriptor.layers[..2] {
        layer.nb_planes = 1;
        layer.planes[0].pitch = 64;
    }
    descriptor
}

#[test]
fn shared_dma_buf_objects_are_valid_and_invalid_plane_references_are_rejected() {
    let mut descriptor = descriptor();
    validate_descriptor(&descriptor, 2).unwrap();
    descriptor.layers[1].planes[0].object_index = 1;
    assert!(validate_descriptor(&descriptor, 2).is_err());
    descriptor.layers[1].planes[0].object_index = -1;
    assert!(validate_descriptor(&descriptor, 2).is_err());
}

#[test]
fn malformed_dma_buf_counts_and_layouts_are_rejected_before_import() {
    let mut invalid = descriptor();
    invalid.nb_objects = 5;
    assert!(validate_descriptor(&invalid, 2).is_err());
    invalid = descriptor();
    invalid.nb_layers = -1;
    assert!(validate_descriptor(&invalid, 2).is_err());
    invalid = descriptor();
    invalid.layers[0].nb_planes = 2;
    assert!(validate_descriptor(&invalid, 2).is_err());
    invalid = descriptor();
    invalid.layers[0].planes[0].pitch = 0;
    assert!(validate_descriptor(&invalid, 2).is_err());
    invalid = descriptor();
    invalid.objects[0].fd = -1;
    assert!(validate_descriptor(&invalid, 2).is_err());
}

#[test]
#[cfg(unix)]
fn missing_dma_buf_size_is_queried_without_closing_the_exported_fd() {
    use std::os::fd::AsRawFd;
    let file = tempfile::tempfile().unwrap();
    file.set_len(8192).unwrap();
    assert_eq!(dma_buf_size(file.as_raw_fd(), 0).unwrap(), 8192);
    assert_eq!(file.metadata().unwrap().len(), 8192);
    assert_eq!(dma_buf_size(-1, 4096).unwrap(), 4096);
    file.set_len(0).unwrap();
    assert!(dma_buf_size(file.as_raw_fd(), 0).is_err());
}
