use std::{
    os::raw::{c_int, c_void},
    slice,
};

use ffmpeg_sys_next as ffi;

use super::super::HTTP_CACHE_MAX_READ_CHUNK_BYTES;
use super::cache::{CacheReadResult, HttpRingCache};

pub(super) struct CachedAvioReader {
    pub(super) cache: HttpRingCache,
    pub(super) read_pos: u64,
}

pub(super) unsafe extern "C" fn cached_avio_read_packet(
    opaque: *mut c_void,
    buf: *mut u8,
    buf_size: c_int,
) -> c_int {
    if opaque.is_null() || buf.is_null() || buf_size <= 0 {
        return ffi::AVERROR(ffi::EINVAL);
    }
    let reader = unsafe { &mut *(opaque as *mut CachedAvioReader) };
    let output_len = (buf_size as usize).min(HTTP_CACHE_MAX_READ_CHUNK_BYTES);
    let output = unsafe { slice::from_raw_parts_mut(buf, output_len) };
    match reader.cache.read_at(reader.read_pos, output) {
        CacheReadResult::Data(read) => {
            if read < output.len() {
                tracing::trace!(
                    read_pos = reader.read_pos,
                    read,
                    requested = output.len(),
                    next_read_pos = reader.read_pos.saturating_add(read as u64),
                    "cached FFmpeg AVIO read returned short data"
                );
            }
            reader.read_pos = reader.read_pos.saturating_add(read as u64);
            c_int::try_from(read).unwrap_or(c_int::MAX)
        }
        CacheReadResult::Eof => {
            tracing::debug!(
                read_pos = reader.read_pos,
                requested = output.len(),
                "cached FFmpeg AVIO read reached EOF"
            );
            ffi::AVERROR_EOF
        }
        #[cfg(test)]
        CacheReadResult::WouldBlock => ffi::AVERROR(ffi::EAGAIN),
        CacheReadResult::Interrupted => {
            tracing::debug!(
                read_pos = reader.read_pos,
                requested = output.len(),
                "cached FFmpeg AVIO read interrupted during shutdown"
            );
            ffi::AVERROR_EXIT
        }
        CacheReadResult::Error(error) => {
            tracing::warn!(%error, "cached FFmpeg AVIO read failed");
            ffi::AVERROR(ffi::EIO)
        }
    }
}

pub(super) unsafe extern "C" fn cached_avio_seek(
    opaque: *mut c_void,
    offset: i64,
    whence: c_int,
) -> i64 {
    if opaque.is_null() {
        return i64::from(ffi::AVERROR(ffi::EINVAL));
    }
    let reader = unsafe { &mut *(opaque as *mut CachedAvioReader) };
    let seek_mode = whence & !ffi::AVSEEK_FORCE;
    if seek_mode == ffi::AVSEEK_SIZE {
        return reader
            .cache
            .content_len()
            .and_then(|len| i64::try_from(len).ok())
            .unwrap_or_else(|| i64::from(ffi::AVERROR(ffi::EIO)));
    }

    let next = match seek_mode {
        value if value == ffi::SEEK_SET => Some(offset),
        value if value == ffi::SEEK_CUR => i64::try_from(reader.read_pos)
            .ok()
            .and_then(|position| position.checked_add(offset)),
        value if value == ffi::SEEK_END => reader
            .cache
            .content_len()
            .and_then(|len| i64::try_from(len).ok())
            .and_then(|len| len.checked_add(offset)),
        _ => None,
    };
    let Some(next) = next else {
        return i64::from(ffi::AVERROR(ffi::EINVAL));
    };
    if next < 0 {
        return i64::from(ffi::AVERROR(ffi::EINVAL));
    }
    let next = next as u64;
    let previous_read_pos = reader.read_pos;
    reader.read_pos = next;
    let range_kind = reader
        .cache
        .range_kind_for_seek(next, seek_mode == ffi::SEEK_END);
    tracing::debug!(
        previous_read_pos,
        next_read_pos = next,
        offset,
        whence,
        seek_mode,
        ?range_kind,
        "cached FFmpeg AVIO seek"
    );
    reader.cache.note_reader_offset(next, range_kind);
    i64::try_from(next).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::{c_int, c_void},
        sync::mpsc,
        thread,
        time::Duration,
    };

    use ffmpeg_sys_next as ffi;

    use super::super::{HttpRingCache, HttpRingCacheState};
    use super::{CachedAvioReader, cached_avio_read_packet, cached_avio_seek};

    #[test]
    fn file_tail_seeks_after_probe_follow_the_main_playback_response() {
        for whence in [ffi::SEEK_SET | ffi::AVSEEK_FORCE, ffi::SEEK_END] {
            let len = 128 * 1024 * 1024;
            let target = len - 4096;
            let mut state = HttpRingCacheState::new(0).with_content_len_hint(Some(len));
            assert!(state.append_at(0, &[1; 1024]));
            let cache = HttpRingCache::from_state_for_test(state);
            cache.finish_metadata_probe();
            let mut reader = Box::new(CachedAvioReader {
                cache: cache.clone(),
                read_pos: 0,
            });
            let offset = if whence == ffi::SEEK_END {
                -4096
            } else {
                target as i64
            };
            let result = unsafe {
                cached_avio_seek(
                    (&mut *reader as *mut CachedAvioReader).cast::<c_void>(),
                    offset,
                    whence,
                )
            };
            assert_eq!(result, target as i64);
            assert_eq!(
                cache.shared_for_download_test().take_restart_offset(),
                Some(target)
            );
            assert!(cache.side_download_requests_for_test().is_empty());
        }
    }

    #[test]
    fn opening_tail_probe_preserves_main_response_and_is_cancelled_when_probe_finishes() {
        let len = 128 * 1024 * 1024;
        let target = len - 4096;
        let mut state = HttpRingCacheState::new(0).with_content_len_hint(Some(len));
        assert!(state.append_at(0, &[1; 1024]));
        let cache = HttpRingCache::from_state_for_test(state);
        let mut reader = Box::new(CachedAvioReader {
            cache: cache.clone(),
            read_pos: 0,
        });
        assert_eq!(
            unsafe {
                cached_avio_seek(
                    (&mut *reader as *mut CachedAvioReader).cast::<c_void>(),
                    target as i64,
                    ffi::SEEK_SET,
                )
            },
            target as i64
        );
        let _ = cache.read_cached_at(target, &mut [0; 1]);
        assert_eq!(cache.shared_for_download_test().download_offset(), 1024);
        assert!(
            cache
                .shared_for_download_test()
                .take_restart_offset()
                .is_none()
        );
        assert_eq!(cache.side_download_requests_for_test().len(), 1);
        cache.finish_metadata_probe();
        assert!(cache.side_download_requests_for_test().is_empty());
    }

    #[test]
    fn pending_cached_seek_does_not_inject_eio_from_avio_callback() {
        let cache = HttpRingCache::from_state_for_test(
            HttpRingCacheState::new(0).with_content_len_hint(Some(1_000)),
        );
        let control = cache.control_for_test();
        let reader = Box::into_raw(Box::new(CachedAvioReader {
            cache: cache.clone(),
            read_pos: 0,
        }));
        let reader_address = reader as usize;
        let (started_tx, started_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let callback = thread::spawn(move || {
            let mut output = [0_u8; 1];
            started_tx
                .send(())
                .expect("AVIO callback start signal sends");
            let result = unsafe {
                cached_avio_read_packet(
                    reader_address as *mut c_void,
                    output.as_mut_ptr(),
                    output.len() as i32,
                )
            };
            result_tx
                .send((result, output))
                .expect("AVIO callback result signal sends");
        });
        started_rx.recv().expect("AVIO callback starts");

        let seek_generation = control.request_seek();
        assert!(
            result_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "pending cached seek must leave the FFmpeg AVIO callback blocked"
        );

        control.finish_seek(seek_generation);
        let _ = cache.shared_for_download_test().append_or_restart(0, b"x");
        let (result, output) = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cached byte wakes the FFmpeg AVIO callback");
        assert_eq!(result, 1);
        assert_ne!(result, ffi::AVERROR(ffi::EIO));
        assert_eq!(output, *b"x");

        callback.join().expect("AVIO callback thread joins");
        unsafe { drop(Box::from_raw(reader)) };
    }

    #[test]
    fn shutdown_interrupt_returns_ffmpeg_exit_instead_of_io_error() {
        let cache = HttpRingCache::from_state_for_test(HttpRingCacheState::new(0));
        cache.control_for_test().shutdown();
        let mut reader = Box::new(CachedAvioReader { cache, read_pos: 0 });
        let mut output = [0_u8; 1];

        let result = unsafe {
            cached_avio_read_packet(
                (&mut *reader as *mut CachedAvioReader).cast::<c_void>(),
                output.as_mut_ptr(),
                output.len() as c_int,
            )
        };

        assert_eq!(result, ffi::AVERROR_EXIT);
        assert_ne!(result, ffi::AVERROR(ffi::EIO));
    }
}
