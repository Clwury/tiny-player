use super::{
    cache::{CacheReadResult, HttpCacheRangeKind, HttpRingCache},
    *,
};

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
    let output = unsafe { slice::from_raw_parts_mut(buf, buf_size as usize) };
    match reader.cache.read_at(reader.read_pos, output) {
        CacheReadResult::Data(read) => {
            reader.read_pos = reader.read_pos.saturating_add(read as u64);
            c_int::try_from(read).unwrap_or(c_int::MAX)
        }
        CacheReadResult::Eof => ffi::AVERROR_EOF,
        CacheReadResult::Interrupted => ffi::AVERROR(ffi::EIO),
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
    reader.read_pos = next;
    let range_kind = if seek_mode == ffi::SEEK_END
        || (seek_mode == ffi::SEEK_SET && reader.cache.is_tail_metadata_probe_seek(next))
    {
        HttpCacheRangeKind::TailMetadataProbe
    } else {
        HttpCacheRangeKind::Playback
    };
    reader.cache.note_reader_offset(next, range_kind);
    i64::try_from(next).unwrap_or(i64::MAX)
}
