use super::*;

impl AvFrame {
    pub(in super::super) fn new() -> std::result::Result<Self, String> {
        let ptr = unsafe { ffi::av_frame_alloc() };
        if ptr.is_null() {
            return Err("FFmpeg 分配 frame 失败".to_string());
        }
        Ok(Self { ptr })
    }

    pub(in super::super) fn as_mut_ptr(&mut self) -> *mut ffi::AVFrame {
        self.ptr
    }

    pub(in super::super) fn unref(&mut self) {
        unsafe { ffi::av_frame_unref(self.ptr) };
    }
}
