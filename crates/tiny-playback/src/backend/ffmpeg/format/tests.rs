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

fn wave() -> Vec<u8> {
    let data_len = 256 * 1024_u32;
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

#[test]
fn native_http_fallback_counts_real_avio_download_bytes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
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
        let bytes = wave();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).unwrap();
        let _ = stream.write_all(&bytes);
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
    for _ in 0..128 {
        let result = unsafe { ffi::av_read_frame(input.as_mut_ptr(), packet.as_mut_ptr()) };
        packet.unref();
        if result < 0 || input.native_http_bytes_read().unwrap() > initial {
            break;
        }
    }
    assert!(input.native_http_bytes_read().unwrap() > initial);
    drop(input);
    server.join().unwrap();
}

#[test]
fn local_avio_bytes_are_not_reported_as_network_downloads() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(&wave()).unwrap();
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
