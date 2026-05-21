use std::{
    mem,
    ops::Deref,
    sync::{Arc, Mutex},
};

use super::{DecodedFrame, PlaybackSessionId, RenderSize, VulkanDecodeDevice};

#[derive(Clone, Default, Debug)]
pub struct FrameBufferPool {
    inner: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl FrameBufferPool {
    const MAX_RETAINED_BUFFERS: usize = 12;

    pub fn rent(&self, min_capacity: usize) -> PooledBytes {
        let mut buffers = self.inner.lock().expect("frame buffer pool poisoned");
        let index = buffers
            .iter()
            .position(|buffer| buffer.capacity() >= min_capacity)
            .unwrap_or_else(|| buffers.len());
        let mut bytes = if index < buffers.len() {
            buffers.swap_remove(index)
        } else {
            Vec::with_capacity(min_capacity)
        };
        bytes.clear();
        PooledBytes {
            bytes,
            pool: Some(self.clone()),
        }
    }

    fn recycle(&self, mut bytes: Vec<u8>) {
        bytes.clear();
        let mut buffers = self.inner.lock().expect("frame buffer pool poisoned");
        if buffers.len() < Self::MAX_RETAINED_BUFFERS {
            buffers.push(bytes);
        }
    }
}

#[derive(Debug)]
pub struct PooledBytes {
    bytes: Vec<u8>,
    pool: Option<FrameBufferPool>,
}

impl PooledBytes {
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        Self { bytes, pool: None }
    }

    pub fn extend_from_slice(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.bytes.as_mut_ptr()
    }

    pub fn resize(&mut self, len: usize, value: u8) {
        self.bytes.resize(len, value);
    }

    pub fn into_vec(mut self) -> Vec<u8> {
        self.pool = None;
        mem::take(&mut self.bytes)
    }
}

impl Clone for PooledBytes {
    fn clone(&self) -> Self {
        Self::from_vec(self.bytes.clone())
    }
}

impl Deref for PooledBytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

impl Drop for PooledBytes {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.recycle(mem::take(&mut self.bytes));
        }
    }
}

impl From<Vec<u8>> for PooledBytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self::from_vec(bytes)
    }
}

impl PartialEq for PooledBytes {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for PooledBytes {}

#[derive(Clone, Default)]
pub struct FrameSlot {
    inner: Arc<Mutex<FrameSlotState>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderBackpressure {
    pub rendering: bool,
    pub pending_requests: usize,
    pub last_render_nsecs: u64,
    pub average_render_nsecs: u64,
}

impl RenderBackpressure {
    pub fn should_drop_non_key_frame(self) -> bool {
        self.is_backlogged()
    }

    pub fn is_backlogged(self) -> bool {
        self.rendering
            && self.pending_requests > 0
            && (self.average_render_nsecs >= 33_000_000 || self.last_render_nsecs >= 50_000_000)
    }
}

#[derive(Default)]
struct FrameSlotState {
    active_session_id: PlaybackSessionId,
    latest_frame: Option<DecodedFrame>,
    current_size: Option<RenderSize>,
    pending_size_change: Option<RenderSize>,
    pending_vulkan_prewarm: Option<Arc<VulkanDecodeDevice>>,
    buffer_pool: FrameBufferPool,
    render_backpressure: RenderBackpressure,
}

impl FrameSlot {
    pub fn buffer_pool(&self) -> FrameBufferPool {
        self.inner
            .lock()
            .expect("video frame slot poisoned")
            .buffer_pool
            .clone()
    }

    pub fn begin_session(&self, session_id: PlaybackSessionId) {
        let mut state = self.inner.lock().expect("video frame slot poisoned");
        let buffer_pool = state.buffer_pool.clone();
        let render_backpressure = state.render_backpressure;
        *state = FrameSlotState {
            active_session_id: session_id,
            latest_frame: None,
            current_size: None,
            pending_size_change: None,
            pending_vulkan_prewarm: None,
            buffer_pool,
            render_backpressure,
        };
    }

    pub fn push(&self, session_id: PlaybackSessionId, frame: DecodedFrame) -> bool {
        let mut state = self.inner.lock().expect("video frame slot poisoned");
        if state.active_session_id != session_id {
            return false;
        }
        if state.current_size != Some(frame.size) {
            state.current_size = Some(frame.size);
            state.pending_size_change = Some(frame.size);
        }
        state.latest_frame = Some(frame);
        true
    }

    pub fn take_frame(&self) -> Option<DecodedFrame> {
        self.inner
            .lock()
            .expect("video frame slot poisoned")
            .latest_frame
            .take()
    }

    pub fn take_size_change(&self) -> Option<(PlaybackSessionId, RenderSize)> {
        let mut state = self.inner.lock().expect("video frame slot poisoned");
        let size = state.pending_size_change.take()?;
        Some((state.active_session_id, size))
    }

    pub fn request_vulkan_prewarm(
        &self,
        session_id: PlaybackSessionId,
        device: Arc<VulkanDecodeDevice>,
    ) {
        let mut state = self.inner.lock().expect("video frame slot poisoned");
        if state.active_session_id == session_id {
            state.pending_vulkan_prewarm = Some(device);
        }
    }

    pub fn take_vulkan_prewarm(&self) -> Option<(PlaybackSessionId, Arc<VulkanDecodeDevice>)> {
        let mut state = self.inner.lock().expect("video frame slot poisoned");
        let device = state.pending_vulkan_prewarm.take()?;
        Some((state.active_session_id, device))
    }

    pub fn render_backpressure(&self) -> RenderBackpressure {
        self.inner
            .lock()
            .expect("video frame slot poisoned")
            .render_backpressure
    }

    pub fn update_render_backpressure(&self, backpressure: RenderBackpressure) {
        self.inner
            .lock()
            .expect("video frame slot poisoned")
            .render_backpressure = backpressure;
    }

    pub fn clear(&self) {
        let mut state = self.inner.lock().expect("video frame slot poisoned");
        let buffer_pool = state.buffer_pool.clone();
        *state = FrameSlotState {
            buffer_pool,
            ..FrameSlotState::default()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::{FrameSlot, RenderBackpressure};
    use crate::player::render_host::{
        DecodedFrame, FfmpegAvBufferRef, FramePixels, PlaybackSessionId, RenderSize,
        VulkanDecodeDevice, VulkanDecodeQueue, VulkanDecodeQueues,
    };
    use std::ptr;

    #[test]
    fn frame_slot_keeps_latest_frame_and_reports_size_changes() {
        let slot = FrameSlot::default();
        let session_id = PlaybackSessionId(1);
        slot.begin_session(session_id);
        let first = RenderSize {
            width: 2,
            height: 1,
        };
        let second = RenderSize {
            width: 4,
            height: 1,
        };

        assert!(slot.push(
            session_id,
            DecodedFrame {
                size: first,
                pts: None,
                key_frame: false,
                pixels: FramePixels::Bgra8(vec![1; 8].into()),
            }
        ));
        assert!(slot.push(
            session_id,
            DecodedFrame {
                size: first,
                pts: None,
                key_frame: false,
                pixels: FramePixels::Bgra8(vec![2; 8].into()),
            }
        ));

        assert_eq!(slot.take_size_change(), Some((session_id, first)));
        assert_eq!(
            slot.take_frame().unwrap().pixels,
            FramePixels::Bgra8(vec![2; 8].into())
        );
        assert!(slot.take_frame().is_none());

        assert!(slot.push(
            session_id,
            DecodedFrame {
                size: second,
                pts: None,
                key_frame: false,
                pixels: FramePixels::Bgra8(vec![3; 16].into()),
            }
        ));
        assert_eq!(slot.take_size_change(), Some((session_id, second)));
    }

    #[test]
    fn frame_slot_rejects_stale_session_frames() {
        let slot = FrameSlot::default();
        let current = PlaybackSessionId(2);
        let stale = PlaybackSessionId(1);
        let size = RenderSize {
            width: 2,
            height: 1,
        };
        slot.begin_session(current);

        assert!(!slot.push(
            stale,
            DecodedFrame {
                size,
                pts: None,
                key_frame: false,
                pixels: FramePixels::Bgra8(vec![1; 8].into()),
            }
        ));

        assert!(slot.take_frame().is_none());
        assert_eq!(slot.take_size_change(), None);
    }

    #[test]
    fn frame_slot_scopes_vulkan_prewarm_to_active_session() {
        let slot = FrameSlot::default();
        let current = PlaybackSessionId(2);
        let stale = PlaybackSessionId(1);
        slot.begin_session(current);
        slot.request_vulkan_prewarm(stale, dummy_vulkan_device(1));
        assert!(slot.take_vulkan_prewarm().is_none());

        let device = dummy_vulkan_device(2);
        slot.request_vulkan_prewarm(current, device.clone());

        let (session_id, requested) = slot.take_vulkan_prewarm().unwrap();
        assert_eq!(session_id, current);
        assert_eq!(requested.key(), device.key());
        assert!(slot.take_vulkan_prewarm().is_none());
    }

    #[test]
    fn render_backpressure_only_drops_non_key_frames_when_backlogged() {
        assert!(!RenderBackpressure::default().should_drop_non_key_frame());
        assert!(
            RenderBackpressure {
                rendering: true,
                pending_requests: 1,
                last_render_nsecs: 55_000_000,
                average_render_nsecs: 20_000_000,
            }
            .should_drop_non_key_frame()
        );
    }

    fn dummy_vulkan_device(device: usize) -> std::sync::Arc<VulkanDecodeDevice> {
        std::sync::Arc::new(VulkanDecodeDevice {
            device_ref: FfmpegAvBufferRef {
                ptr: ptr::null_mut(),
            },
            instance: 0,
            get_proc_addr: 0,
            physical_device: 0,
            device,
            extensions: 0,
            num_extensions: 0,
            features: 0,
            queues: VulkanDecodeQueues {
                graphics: VulkanDecodeQueue { index: 0, count: 1 },
                compute: None,
                transfer: None,
            },
        })
    }
}
