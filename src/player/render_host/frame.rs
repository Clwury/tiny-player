use super::{
    FrameColor, PooledBytes, RawVideoChromaSite, RawVideoFormat, RawVideoPlanes, RawVideoRange,
    VulkanDecodeDevice, VulkanVideoFrame,
};
use crate::player::{dovi::DoviFrameMetadata, ffmpeg_dovi::FfmpegDoviMetadata};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlaybackSessionId(pub u64);

impl PlaybackSessionId {
    pub fn next(self) -> Self {
        Self(self.0.wrapping_add(1).max(1))
    }
}

#[derive(Clone, Debug)]
pub struct DecodedFrame {
    pub size: RenderSize,
    pub pts: Option<FramePts>,
    pub key_frame: bool,
    pub pixels: FramePixels,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FramePts {
    pub nsecs: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FramePixels {
    Bgra8(PooledBytes),
    RawVideo(RawVideoFrame),
    VulkanVideo(VulkanVideoFrame),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawVideoFrame {
    pub format: RawVideoFormat,
    pub color: FrameColor,
    pub range: RawVideoRange,
    pub chroma_site: RawVideoChromaSite,
    pub metadata: Option<FrameDynamicMetadata>,
    pub planes: RawVideoPlanes,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameDynamicMetadata {
    pub dolby_vision: Option<DoviFrameMetadata>,
    pub ffmpeg_dovi: Option<FfmpegDoviMetadata>,
}

#[allow(dead_code)]
fn _assert_vulkan_device_is_reexported(_: &VulkanDecodeDevice) {}
