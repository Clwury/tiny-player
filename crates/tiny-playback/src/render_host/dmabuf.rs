use std::sync::Arc;

use super::{
    FfmpegFrameRef, FrameColor, FrameDynamicMetadata, RawVideoChromaSite, RawVideoFormat,
    RawVideoRange, VulkanDecodeDevice,
};

/// Owns an exported DRM PRIME frame, its DMA-BUF FDs and its VA-API surface.
/// Vulkan import is scoped to each render and is never cached across frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DmaBufVideoFrame {
    pub frame: FfmpegFrameRef,
    pub device: Arc<VulkanDecodeDevice>,
    pub format: RawVideoFormat,
    pub color: FrameColor,
    pub range: RawVideoRange,
    pub chroma_site: RawVideoChromaSite,
    pub metadata: Option<FrameDynamicMetadata>,
}
