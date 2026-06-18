use std::{ffi::CStr, os::raw::c_int};

use ffmpeg_sys_next as ffi;

pub(super) fn ffmpeg_error(error: c_int) -> String {
    let mut buffer = [0i8; 256];
    let result = unsafe { ffi::av_strerror(error, buffer.as_mut_ptr(), buffer.len()) };
    if result < 0 {
        return format!("FFmpeg error {error}");
    }
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}
