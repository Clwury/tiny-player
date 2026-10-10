use super::*;

#[test]
fn h264_and_hevc_invalid_packets_do_not_abort_decoding_or_swallow_resource_errors() {
    for decoder in [Decoder::open_h264_for_test(), Decoder::open_hevc_for_test()] {
        assert_eq!(
            unsafe { (*decoder.ptr).err_recognition } & ffi::AV_EF_EXPLODE,
            0
        );
        let props = AvPacket::new().unwrap();
        let invalid = AvPacket::from_data_and_props(&[0xff; 4], &props).unwrap();
        let mut frame = AvFrame::new().unwrap();
        for _ in 0..2 {
            decoder
                .decode_packet(invalid.as_ptr(), &mut frame, |_| {
                    panic!("an invalid NAL must not produce a frame")
                })
                .expect("invalid video input must allow the next packet");
        }
        for operation in ["send_packet", "receive_frame"] {
            assert!(decoder.continue_after_video_invalid_data(ffi::AVERROR_INVALIDDATA, operation));
            for error in [
                ffi::AVERROR(ffi::ENOMEM),
                ffi::AVERROR(ffi::EIO),
                ffi::AVERROR_EXTERNAL,
                ffi::AVERROR_EOF,
            ] {
                assert!(!decoder.continue_after_video_invalid_data(error, operation));
            }
        }
    }
}

#[test]
fn hevc_invalid_sei_preserves_slice_and_following_reference_frames() {
    assert_eq!(
        decode_hevc_reference_chain(true, false),
        (0..24).map(|index| index * 40).collect::<Vec<_>>()
    );
}

#[test]
fn hevc_invalid_packet_keeps_buffered_output_and_following_input() {
    assert_eq!(
        decode_hevc_reference_chain(false, true),
        (0..24).map(|index| index * 40).collect::<Vec<_>>()
    );
}

fn decode_hevc_reference_chain(invalid_sei: bool, invalid_packet: bool) -> Vec<i64> {
    // 24 gray 64x64 frames: one IDR followed exclusively by dependent P frames.
    // Generated with ffmpeg's color source at 25 fps and libx265 ultrafast:
    // -x265-params pools=none:frame-threads=1:keyint=120:min-keyint=120:
    // scenecut=0:bframes=0:aud=1:repeat-headers=1:info=0 -frames:v 24 -f hevc
    let data = include_bytes!("data/hevc_reference_chain.hevc");
    let aud = [0, 0, 0, 1, 0x46, 0x01];
    let mut boundaries = data
        .windows(aud.len())
        .enumerate()
        .filter_map(|(offset, bytes)| (bytes == aud).then_some(offset))
        .collect::<Vec<_>>();
    assert_eq!(boundaries.len(), 24);
    boundaries.push(data.len());

    let decoder = Decoder::open_hevc_for_test();
    let mut frame = AvFrame::new().unwrap();
    let mut timestamps = Vec::new();
    let mut record_frame = |frame: *mut ffi::AVFrame| {
        assert_eq!(unsafe { (*frame).decode_error_flags }, 0);
        assert_eq!(unsafe { (*frame).flags } & ffi::AV_FRAME_FLAG_CORRUPT, 0);
        timestamps.push(unsafe { (*frame).pts });
        Ok(())
    };

    for (index, offsets) in boundaries.windows(2).enumerate() {
        let original = &data[offsets[0]..offsets[1]];
        let mut payload = original.to_vec();
        if invalid_sei && matches!(index, 3 | 17) {
            // Prefix SEI (NAL 39) advertises 255 payload bytes but contains
            // only a trailing bit. Keep the valid slice in the same AU.
            payload.splice(7..7, [0, 0, 0, 1, 0x4e, 0x01, 0x01, 0xff, 0x80]);
        }
        let mut props = AvPacket::new().unwrap();
        unsafe {
            (*props.as_mut_ptr()).pts = index as i64 * 40;
            (*props.as_mut_ptr()).dts = index as i64 * 40;
            (*props.as_mut_ptr()).duration = 40;
            (*props.as_mut_ptr()).flags = if index == 0 { ffi::AV_PKT_FLAG_KEY } else { 0 };
        }
        let packet = AvPacket::from_data_and_props(&payload, &props).unwrap();
        decoder
            .decode_packet(packet.as_ptr(), &mut frame, &mut record_frame)
            .expect("bad metadata must not abort a valid HEVC access unit");
        if invalid_packet && matches!(index, 3 | 17) {
            let invalid = AvPacket::from_data_and_props(&[0xff; 4], &props).unwrap();
            decoder
                .decode_packet(invalid.as_ptr(), &mut frame, &mut record_frame)
                .expect("a bad HEVC packet must retain buffered frames and references");
        }
    }
    decoder.flush(&mut frame, &mut record_frame).unwrap();
    timestamps
}
