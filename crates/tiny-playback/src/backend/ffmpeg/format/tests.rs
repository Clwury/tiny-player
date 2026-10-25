use std::{
    io::{BufRead as _, BufReader, Write as _},
    net::TcpListener,
    time::Duration,
};

use super::*;
use crate::backend::ffmpeg::{
    AvPacket,
    avio::{HttpRingCache, HttpRingCacheState},
};
use crate::render_host::PlaybackSessionId;

fn wave(data_len: u32) -> Vec<u8> {
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&1_u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&48_000_u32.to_le_bytes());
    bytes.extend_from_slice(&96_000_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    bytes.resize(bytes.len() + data_len as usize, 0);
    bytes
}

fn native_http_download_bytes(data_len: u32) -> (u64, u64) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let bytes = wave(data_len);
    let expected_bytes = bytes.len() as u64;
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                break;
            }
        }
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).unwrap();
        stream.write_all(&bytes).unwrap();
    });
    let cache = HttpRingCache::from_state_for_test(HttpRingCacheState::new_with_cache_capacity(
        0,
        64 * 1024,
    ));
    let mut source = CachedInputSource::from_cache_for_test(cache);
    assert!(source.disable_empty_startup_cache_for_native_fallback());
    let mut input = FormatContext::open(
        &format!("http://{address}/fixture.wav"),
        &[],
        InputProbeProfile::Fast,
        &source,
        Arc::new(FfmpegControl::new(PlaybackSessionId(1))),
    )
    .unwrap();
    assert!(input.cached_io_cache().is_none());
    let initial = input
        .native_http_bytes_read()
        .expect("native HTTP has an AVIO counter");
    assert!(initial > 0);
    let mut packet = AvPacket::new().unwrap();
    let mut packets = 0;
    let mut previous = initial;
    // Opening/probing can prefetch the whole body. Packet sizes and the
    // number of packets needed to exhaust AVIO's buffer vary by FFmpeg build.
    loop {
        let result = unsafe { ffi::av_read_frame(input.as_mut_ptr(), packet.as_mut_ptr()) };
        if result == ffi::AVERROR_EOF {
            break;
        }
        assert!(
            result >= 0,
            "HTTP fixture read failed: {}",
            ffmpeg_error(result)
        );
        packets += 1;
        packet.unref();
        let current = input.native_http_bytes_read().unwrap();
        assert!(current >= previous, "HTTP byte counter moved backwards");
        previous = current;
    }
    assert!(packets > 0);
    assert_eq!(input.native_http_bytes_read(), Some(expected_bytes));
    drop(input);
    server.join().unwrap();
    (initial, expected_bytes)
}

#[test]
fn native_http_fallback_counts_real_avio_download_bytes() {
    native_http_download_bytes(256 * 1024);
}

#[test]
fn native_http_counter_remains_valid_when_opening_prefetches_the_entire_body() {
    let (initial, total) = native_http_download_bytes(128);
    assert_eq!(initial, total);
}

#[test]
fn local_avio_bytes_are_not_reported_as_network_downloads() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&wave(256 * 1024)).unwrap();
    let input = FormatContext::open_with_cached_io(
        file.path().to_str().unwrap(),
        &[],
        InputProbeProfile::Fast,
        Arc::new(FfmpegControl::new(PlaybackSessionId(1))),
        None,
    )
    .unwrap();
    assert!(input.native_http_bytes_read().is_none());
}

#[test]
fn input_retains_interrupt_control_until_the_format_context_is_closed() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&wave(128)).unwrap();
    let control = Arc::new(FfmpegControl::new(PlaybackSessionId(1)));
    let weak = Arc::downgrade(&control);
    let input = FormatContext::open_with_cached_io(
        file.path().to_str().unwrap(),
        &[],
        InputProbeProfile::Fast,
        control,
        None,
    )
    .unwrap();
    assert!(weak.upgrade().is_some());
    drop(input);
    assert!(weak.upgrade().is_none());
}
