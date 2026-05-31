use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use super::{DecodedFrame, FrameBufferPool, PlaybackSessionId, RenderSize, VulkanDecodeDevice};

#[derive(Clone, Default)]
pub struct VideoOutputQueue {
    inner: Arc<Mutex<VideoOutputQueueState>>,
}

const VIDEO_OUTPUT_QUEUE_CAPACITY: usize = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VideoOutputQueueSnapshot {
    pub active_session_id: PlaybackSessionId,
    pub queued_frames: usize,
    pub queue_capacity: usize,
    pub dropped_frames: u64,
    pub render_backpressure: RenderBackpressure,
}

impl VideoOutputQueueSnapshot {
    pub fn render_backlogged(self) -> bool {
        self.queued_frames > 0 && self.render_backpressure.is_backlogged()
    }

    pub fn blocked_on(self) -> Option<&'static str> {
        if self.render_backlogged() {
            Some("render_worker")
        } else if self.queued_frames >= self.queue_capacity {
            Some("vo_queue")
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum VideoOutputQueuePushPolicy {
    DropOldest,
    DropBackloggedNonKey,
    ReplacePending,
    WouldBlock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoOutputQueuePushResult {
    Queued,
    DroppedOldest,
    DroppedBacklogged,
    ReplacedPending,
    WouldBlock,
    InactiveSession,
}

impl VideoOutputQueuePushResult {
    pub fn accepted(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::DroppedOldest | Self::ReplacedPending
        )
    }

    pub fn dropped_oldest(self) -> bool {
        matches!(self, Self::DroppedOldest)
    }

    pub fn dropped_backlogged(self) -> bool {
        matches!(self, Self::DroppedBacklogged)
    }

    #[allow(dead_code)]
    pub fn replaced_pending(self) -> bool {
        matches!(self, Self::ReplacedPending)
    }

    #[allow(dead_code)]
    pub fn would_block(self) -> bool {
        matches!(self, Self::WouldBlock)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoOutputQueueAdmission {
    pub before: VideoOutputQueueSnapshot,
    pub after: VideoOutputQueueSnapshot,
    pub result: VideoOutputQueuePushResult,
    pub replaced_pending_frame: bool,
}

impl VideoOutputQueueAdmission {
    pub fn accepted(self) -> bool {
        self.result.accepted()
    }

    pub fn dropped_backlogged(self) -> bool {
        self.result.dropped_backlogged()
    }
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
        self.rendering && self.render_is_slow()
    }

    pub fn is_backlogged(self) -> bool {
        self.rendering && (self.pending_requests > 0 || self.render_is_slow())
    }

    fn render_is_slow(self) -> bool {
        self.average_render_nsecs >= 33_000_000 || self.last_render_nsecs >= 50_000_000
    }
}

#[derive(Default)]
struct VideoOutputQueueState {
    active_session_id: PlaybackSessionId,
    frames: VecDeque<DecodedFrame>,
    current_size: Option<RenderSize>,
    pending_size_change: Option<RenderSize>,
    pending_vulkan_prewarm: Option<Arc<VulkanDecodeDevice>>,
    buffer_pool: FrameBufferPool,
    render_backpressure: RenderBackpressure,
    dropped_frames: u64,
}

impl VideoOutputQueue {
    pub fn buffer_pool(&self) -> FrameBufferPool {
        self.inner
            .lock()
            .expect("video output queue poisoned")
            .buffer_pool
            .clone()
    }

    pub fn begin_session(&self, session_id: PlaybackSessionId) {
        let mut state = self.inner.lock().expect("video output queue poisoned");
        let buffer_pool = state.buffer_pool.clone();
        let render_backpressure = state.render_backpressure;
        *state = VideoOutputQueueState {
            active_session_id: session_id,
            frames: VecDeque::new(),
            current_size: None,
            pending_size_change: None,
            pending_vulkan_prewarm: None,
            buffer_pool,
            render_backpressure,
            dropped_frames: 0,
        };
    }

    #[allow(dead_code)]
    pub fn push(&self, session_id: PlaybackSessionId, frame: DecodedFrame) -> bool {
        self.push_with_result(session_id, frame).accepted()
    }

    pub fn push_with_result(
        &self,
        session_id: PlaybackSessionId,
        frame: DecodedFrame,
    ) -> VideoOutputQueuePushResult {
        self.push_with_policy(session_id, frame, VideoOutputQueuePushPolicy::DropOldest)
    }

    pub fn admit_presented_frame(
        &self,
        session_id: PlaybackSessionId,
        frame: DecodedFrame,
    ) -> VideoOutputQueueAdmission {
        let before = self.snapshot();
        let result = self.push_with_policy(
            session_id,
            frame,
            VideoOutputQueuePushPolicy::DropBackloggedNonKey,
        );
        VideoOutputQueueAdmission {
            before,
            after: self.snapshot(),
            result,
            replaced_pending_frame: before.queued_frames > 0,
        }
    }

    pub fn push_with_policy(
        &self,
        session_id: PlaybackSessionId,
        frame: DecodedFrame,
        policy: VideoOutputQueuePushPolicy,
    ) -> VideoOutputQueuePushResult {
        let mut state = self.inner.lock().expect("video output queue poisoned");
        if state.active_session_id != session_id {
            return VideoOutputQueuePushResult::InactiveSession;
        }
        if matches!(policy, VideoOutputQueuePushPolicy::DropBackloggedNonKey)
            && !frame.key_frame
            && !state.frames.is_empty()
            && state.render_backpressure.should_drop_non_key_frame()
        {
            state.dropped_frames = state.dropped_frames.saturating_add(1);
            return VideoOutputQueuePushResult::DroppedBacklogged;
        }
        if state.current_size != Some(frame.size) {
            state.current_size = Some(frame.size);
            state.pending_size_change = Some(frame.size);
        }
        let mut result = VideoOutputQueuePushResult::Queued;
        if state.frames.len() >= VIDEO_OUTPUT_QUEUE_CAPACITY {
            match policy {
                VideoOutputQueuePushPolicy::DropOldest
                | VideoOutputQueuePushPolicy::DropBackloggedNonKey => {
                    state.frames.pop_front();
                    state.dropped_frames = state.dropped_frames.saturating_add(1);
                    result = VideoOutputQueuePushResult::DroppedOldest;
                }
                VideoOutputQueuePushPolicy::ReplacePending => {
                    state.frames.pop_back();
                    state.dropped_frames = state.dropped_frames.saturating_add(1);
                    result = VideoOutputQueuePushResult::ReplacedPending;
                }
                VideoOutputQueuePushPolicy::WouldBlock => {
                    return VideoOutputQueuePushResult::WouldBlock;
                }
            }
        }
        state.frames.push_back(frame);
        result
    }

    #[allow(dead_code)]
    pub fn take_frame(&self) -> Option<DecodedFrame> {
        self.take_next_frame()
    }

    pub fn take_next_frame(&self) -> Option<DecodedFrame> {
        self.inner
            .lock()
            .expect("video output queue poisoned")
            .frames
            .pop_front()
    }

    pub fn snapshot(&self) -> VideoOutputQueueSnapshot {
        let state = self.inner.lock().expect("video output queue poisoned");
        VideoOutputQueueSnapshot {
            active_session_id: state.active_session_id,
            queued_frames: state.frames.len(),
            queue_capacity: VIDEO_OUTPUT_QUEUE_CAPACITY,
            dropped_frames: state.dropped_frames,
            render_backpressure: state.render_backpressure,
        }
    }

    pub fn take_size_change(&self) -> Option<(PlaybackSessionId, RenderSize)> {
        let mut state = self.inner.lock().expect("video output queue poisoned");
        let size = state.pending_size_change.take()?;
        Some((state.active_session_id, size))
    }

    pub fn request_vulkan_prewarm(
        &self,
        session_id: PlaybackSessionId,
        device: Arc<VulkanDecodeDevice>,
    ) {
        let mut state = self.inner.lock().expect("video output queue poisoned");
        if state.active_session_id == session_id {
            state.pending_vulkan_prewarm = Some(device);
        }
    }

    pub fn take_vulkan_prewarm(&self) -> Option<(PlaybackSessionId, Arc<VulkanDecodeDevice>)> {
        let mut state = self.inner.lock().expect("video output queue poisoned");
        let device = state.pending_vulkan_prewarm.take()?;
        Some((state.active_session_id, device))
    }

    pub fn update_render_backpressure(&self, backpressure: RenderBackpressure) {
        self.inner
            .lock()
            .expect("video output queue poisoned")
            .render_backpressure = backpressure;
    }

    pub fn clear(&self) {
        let mut state = self.inner.lock().expect("video output queue poisoned");
        let buffer_pool = state.buffer_pool.clone();
        *state = VideoOutputQueueState {
            buffer_pool,
            ..VideoOutputQueueState::default()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RenderBackpressure, VideoOutputQueue, VideoOutputQueuePushPolicy,
        VideoOutputQueuePushResult,
    };
    use crate::player::render_host::{
        DecodedFrame, FfmpegAvBufferRef, FramePixels, PlaybackSessionId, RenderSize,
        VulkanDecodeDevice, VulkanDecodeQueue, VulkanDecodeQueues,
    };
    use std::ptr;

    #[test]
    fn video_output_queue_queues_frames_and_reports_size_changes() {
        let slot = VideoOutputQueue::default();
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
            FramePixels::Bgra8(vec![1; 8].into())
        );
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
    fn video_output_queue_drops_oldest_frame_when_vo_queue_is_full() {
        let slot = VideoOutputQueue::default();
        let session_id = PlaybackSessionId(1);
        let size = RenderSize {
            width: 2,
            height: 1,
        };
        slot.begin_session(session_id);

        for value in 1..=4 {
            let result = slot.push_with_result(
                session_id,
                DecodedFrame {
                    size,
                    pts: None,
                    key_frame: false,
                    pixels: FramePixels::Bgra8(vec![value; 8].into()),
                },
            );
            if value < 4 {
                assert_eq!(result, VideoOutputQueuePushResult::Queued);
            } else {
                assert_eq!(result, VideoOutputQueuePushResult::DroppedOldest);
            }
        }

        let snapshot = slot.snapshot();
        assert_eq!(snapshot.queued_frames, 3);
        assert_eq!(snapshot.queue_capacity, 3);
        assert_eq!(snapshot.dropped_frames, 1);
        assert_eq!(
            slot.take_frame().unwrap().pixels,
            FramePixels::Bgra8(vec![2; 8].into())
        );
        assert_eq!(
            slot.take_frame().unwrap().pixels,
            FramePixels::Bgra8(vec![3; 8].into())
        );
        assert_eq!(
            slot.take_frame().unwrap().pixels,
            FramePixels::Bgra8(vec![4; 8].into())
        );
        assert!(slot.take_frame().is_none());
    }

    #[test]
    fn video_output_queue_can_report_would_block_when_full() {
        let slot = VideoOutputQueue::default();
        let session_id = PlaybackSessionId(1);
        let size = RenderSize {
            width: 2,
            height: 1,
        };
        slot.begin_session(session_id);

        for value in 1..=3 {
            assert_eq!(
                slot.push_with_policy(
                    session_id,
                    DecodedFrame {
                        size,
                        pts: None,
                        key_frame: false,
                        pixels: FramePixels::Bgra8(vec![value; 8].into()),
                    },
                    VideoOutputQueuePushPolicy::WouldBlock,
                ),
                VideoOutputQueuePushResult::Queued
            );
        }
        let result = slot.push_with_policy(
            session_id,
            DecodedFrame {
                size,
                pts: None,
                key_frame: false,
                pixels: FramePixels::Bgra8(vec![4; 8].into()),
            },
            VideoOutputQueuePushPolicy::WouldBlock,
        );

        assert!(result.would_block());
        assert_eq!(slot.snapshot().queued_frames, 3);
        assert_eq!(slot.snapshot().dropped_frames, 0);
        assert_eq!(
            slot.take_next_frame().unwrap().pixels,
            FramePixels::Bgra8(vec![1; 8].into())
        );
    }

    #[test]
    fn video_output_queue_can_replace_pending_frame_when_full() {
        let slot = VideoOutputQueue::default();
        let session_id = PlaybackSessionId(1);
        let size = RenderSize {
            width: 2,
            height: 1,
        };
        slot.begin_session(session_id);

        for value in 1..=3 {
            assert_eq!(
                slot.push_with_policy(
                    session_id,
                    DecodedFrame {
                        size,
                        pts: None,
                        key_frame: false,
                        pixels: FramePixels::Bgra8(vec![value; 8].into()),
                    },
                    VideoOutputQueuePushPolicy::ReplacePending,
                ),
                VideoOutputQueuePushResult::Queued
            );
        }
        let result = slot.push_with_policy(
            session_id,
            DecodedFrame {
                size,
                pts: None,
                key_frame: false,
                pixels: FramePixels::Bgra8(vec![4; 8].into()),
            },
            VideoOutputQueuePushPolicy::ReplacePending,
        );

        assert!(result.replaced_pending());
        assert_eq!(slot.snapshot().queued_frames, 3);
        assert_eq!(slot.snapshot().dropped_frames, 1);
        assert_eq!(
            slot.take_next_frame().unwrap().pixels,
            FramePixels::Bgra8(vec![1; 8].into())
        );
        assert_eq!(
            slot.take_next_frame().unwrap().pixels,
            FramePixels::Bgra8(vec![2; 8].into())
        );
        assert_eq!(
            slot.take_next_frame().unwrap().pixels,
            FramePixels::Bgra8(vec![4; 8].into())
        );
    }

    #[test]
    fn video_output_queue_rejects_stale_session_frames() {
        let slot = VideoOutputQueue::default();
        let current = PlaybackSessionId(2);
        let stale = PlaybackSessionId(1);
        let size = RenderSize {
            width: 2,
            height: 1,
        };
        slot.begin_session(current);

        assert_eq!(
            slot.push_with_result(
                stale,
                DecodedFrame {
                    size,
                    pts: None,
                    key_frame: false,
                    pixels: FramePixels::Bgra8(vec![1; 8].into()),
                },
            ),
            VideoOutputQueuePushResult::InactiveSession
        );
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
    fn video_output_queue_applies_render_backpressure_drop_policy_at_vo_boundary() {
        let slot = VideoOutputQueue::default();
        let session_id = PlaybackSessionId(1);
        let size = RenderSize {
            width: 2,
            height: 1,
        };
        slot.begin_session(session_id);
        slot.update_render_backpressure(RenderBackpressure {
            rendering: true,
            pending_requests: 1,
            last_render_nsecs: 55_000_000,
            average_render_nsecs: 20_000_000,
        });

        let queued = slot
            .admit_presented_frame(
                session_id,
                DecodedFrame {
                    size,
                    pts: None,
                    key_frame: false,
                    pixels: FramePixels::Bgra8(vec![1; 8].into()),
                },
            )
            .result;
        assert_eq!(queued, VideoOutputQueuePushResult::Queued);

        let dropped = slot
            .admit_presented_frame(
                session_id,
                DecodedFrame {
                    size,
                    pts: None,
                    key_frame: false,
                    pixels: FramePixels::Bgra8(vec![2; 8].into()),
                },
            )
            .result;
        assert_eq!(dropped, VideoOutputQueuePushResult::DroppedBacklogged);
        assert_eq!(slot.snapshot().dropped_frames, 1);

        let queued = slot
            .admit_presented_frame(
                session_id,
                DecodedFrame {
                    size,
                    pts: None,
                    key_frame: true,
                    pixels: FramePixels::Bgra8(vec![3; 8].into()),
                },
            )
            .result;
        assert_eq!(queued, VideoOutputQueuePushResult::Queued);
        assert_eq!(
            slot.take_frame().unwrap().pixels,
            FramePixels::Bgra8(vec![1; 8].into())
        );
        assert_eq!(
            slot.take_frame().unwrap().pixels,
            FramePixels::Bgra8(vec![3; 8].into())
        );
    }

    #[test]
    fn video_output_queue_admission_reports_snapshot_transition() {
        let slot = VideoOutputQueue::default();
        let session_id = PlaybackSessionId(1);
        let size = RenderSize {
            width: 2,
            height: 1,
        };
        slot.begin_session(session_id);
        assert!(slot.push(
            session_id,
            DecodedFrame {
                size,
                pts: None,
                key_frame: false,
                pixels: FramePixels::Bgra8(vec![1; 8].into()),
            }
        ));

        let admission = slot.admit_presented_frame(
            session_id,
            DecodedFrame {
                size,
                pts: None,
                key_frame: false,
                pixels: FramePixels::Bgra8(vec![2; 8].into()),
            },
        );

        assert!(admission.accepted());
        assert_eq!(admission.result, VideoOutputQueuePushResult::Queued);
        assert_eq!(admission.before.queued_frames, 1);
        assert_eq!(admission.after.queued_frames, 2);
        assert!(admission.replaced_pending_frame);
    }

    #[test]
    fn video_output_queue_scopes_vulkan_prewarm_to_active_session() {
        let slot = VideoOutputQueue::default();
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
