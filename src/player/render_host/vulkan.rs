use std::{fmt, ptr, sync::Arc};

use anyhow::{Result, anyhow};
use ffmpeg_sys_next as ffmpeg_ffi;

use super::{FrameColor, FrameDynamicMetadata, RawVideoChromaSite, RawVideoFormat, RawVideoRange};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VulkanVideoFrame {
    pub frame: FfmpegFrameRef,
    pub device: Arc<VulkanDecodeDevice>,
    pub format: RawVideoFormat,
    pub usage: u32,
    pub color: FrameColor,
    pub range: RawVideoRange,
    pub chroma_site: RawVideoChromaSite,
    pub metadata: Option<FrameDynamicMetadata>,
    pub planes: Vec<VulkanVideoPlane>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VulkanVideoPlane {
    pub image: usize,
    pub format: u32,
    pub layout: u32,
    pub queue_family: u32,
    pub semaphore: usize,
    pub semaphore_value: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VulkanDecodeDevice {
    pub(in crate::player::render_host) device_ref: FfmpegAvBufferRef,
    pub instance: usize,
    pub get_proc_addr: usize,
    pub physical_device: usize,
    pub device: usize,
    pub extensions: usize,
    pub num_extensions: i32,
    pub features: usize,
    pub queues: VulkanDecodeQueues,
}

impl VulkanDecodeDevice {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device_ref: FfmpegAvBufferRef,
        instance: usize,
        get_proc_addr: usize,
        physical_device: usize,
        device: usize,
        extensions: usize,
        num_extensions: i32,
        features: usize,
        queues: VulkanDecodeQueues,
    ) -> Self {
        Self {
            device_ref,
            instance,
            get_proc_addr,
            physical_device,
            device,
            extensions,
            num_extensions,
            features,
            queues,
        }
    }

    pub fn key(&self) -> usize {
        self.device
    }

    pub fn device_ref(&self) -> *mut ffmpeg_ffi::AVBufferRef {
        self.device_ref.as_ptr()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VulkanDecodeQueues {
    pub graphics: VulkanDecodeQueue,
    pub compute: Option<VulkanDecodeQueue>,
    pub transfer: Option<VulkanDecodeQueue>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VulkanDecodeQueue {
    pub index: u32,
    pub count: u32,
}

#[derive(Debug)]
pub struct FfmpegFrameRef {
    ptr: *mut ffmpeg_ffi::AVFrame,
}

impl FfmpegFrameRef {
    pub fn new_ref(frame: *const ffmpeg_ffi::AVFrame) -> Result<Self> {
        if frame.is_null() {
            return Err(anyhow!("cannot reference a null FFmpeg frame"));
        }

        let ptr = unsafe { ffmpeg_ffi::av_frame_alloc() };
        if ptr.is_null() {
            return Err(anyhow!("FFmpeg failed to allocate a frame reference"));
        }

        let result = unsafe { ffmpeg_ffi::av_frame_ref(ptr, frame) };
        if result < 0 {
            let mut ptr = ptr;
            unsafe { ffmpeg_ffi::av_frame_free(&mut ptr) };
            return Err(anyhow!("FFmpeg failed to reference a decoded frame"));
        }

        Ok(Self { ptr })
    }

    pub fn as_ptr(&self) -> *const ffmpeg_ffi::AVFrame {
        self.ptr
    }

    pub fn as_mut_ptr(&self) -> *mut ffmpeg_ffi::AVFrame {
        self.ptr
    }
}

impl Clone for FfmpegFrameRef {
    fn clone(&self) -> Self {
        Self::new_ref(self.ptr).expect("failed to clone FFmpeg frame reference")
    }
}

impl PartialEq for FfmpegFrameRef {
    fn eq(&self, other: &Self) -> bool {
        ptr::eq(self.ptr, other.ptr)
    }
}

impl Eq for FfmpegFrameRef {}

impl Drop for FfmpegFrameRef {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffmpeg_ffi::av_frame_free(&mut self.ptr) };
        }
    }
}

unsafe impl Send for FfmpegFrameRef {}
unsafe impl Sync for FfmpegFrameRef {}

#[derive(Debug)]
pub struct FfmpegAvBufferRef {
    pub(in crate::player::render_host) ptr: *mut ffmpeg_ffi::AVBufferRef,
}

impl FfmpegAvBufferRef {
    pub fn new_ref(buffer: *mut ffmpeg_ffi::AVBufferRef) -> Result<Self> {
        if buffer.is_null() {
            return Err(anyhow!("cannot reference a null FFmpeg buffer"));
        }

        let ptr = unsafe { ffmpeg_ffi::av_buffer_ref(buffer) };
        if ptr.is_null() {
            return Err(anyhow!("FFmpeg failed to reference a buffer"));
        }

        Ok(Self { ptr })
    }

    pub fn as_ptr(&self) -> *mut ffmpeg_ffi::AVBufferRef {
        self.ptr
    }
}

impl Clone for FfmpegAvBufferRef {
    fn clone(&self) -> Self {
        Self::new_ref(self.ptr).expect("failed to clone FFmpeg buffer reference")
    }
}

impl PartialEq for FfmpegAvBufferRef {
    fn eq(&self, other: &Self) -> bool {
        ptr::eq(self.ptr, other.ptr)
    }
}

impl Eq for FfmpegAvBufferRef {}

impl Drop for FfmpegAvBufferRef {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { ffmpeg_ffi::av_buffer_unref(&mut self.ptr) };
        }
    }
}

unsafe impl Send for FfmpegAvBufferRef {}
unsafe impl Sync for FfmpegAvBufferRef {}

impl fmt::Display for VulkanDecodeQueue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.index, self.count)
    }
}
