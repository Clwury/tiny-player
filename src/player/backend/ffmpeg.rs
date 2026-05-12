use std::{
    collections::VecDeque,
    env,
    ffi::{CStr, CString},
    io::Read,
    os::raw::{c_int, c_void},
    ptr, slice,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use cpal::{
    FromSample, Sample, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use ffmpeg_sys_next as ffi;

use super::{BackendError, BackendEvent, HttpStreamBufferProgress, Result};
use crate::player::{
    dovi::{DoviFrameMetadata, DoviRpuExtractor, HevcStreamFormat},
    render_host::{
        DecodedFrame, FrameColor, FrameDynamicMetadata, FramePixels, FramePts, FrameSlot,
        RawVideoChromaSite, RawVideoFormat, RawVideoFrame, RawVideoPlane, RawVideoPlanes,
        RawVideoRange, RenderSize,
    },
};

mod audio;
mod avio;
mod clock;
mod codec;
mod dovi;
mod format;
mod playback_loop;
mod reporting;
mod util;
mod video;
mod worker;

#[cfg(test)]
use audio::{
    AudioBuffer, AudioShared, audio_samples_duration, fill_audio_output, samples_for_duration,
};
use audio::{AudioOutput, audio_sample_len, frame_sample_format, zeroed_channel_layout};
#[cfg(test)]
use avio::{
    HttpRingCacheState, content_len_from_content_range, ffmpeg_http_headers,
    http_cache_range_header, http_cache_range_request_len, http_cache_range_request_timeout,
    http_cache_request_headers_for_log, http_cache_response_headers_for_log, reqwest_header_pairs,
    should_cache_http_url,
};
#[cfg(test)]
use clock::{MappedTimestamp, WaitStatus};
use clock::{
    PlaybackScheduler, QueuedVideoFrame, TimestampMapper, drain_audio_clocked_video_queue,
    duration_nsecs, frame_best_effort_timestamp, max_optional_seconds, nsecs_to_timestamp,
    optional_buffered_value_changed, present_decoded_video_frame,
    present_due_audio_clocked_video_frames, pts_distance, queued_video_duration, seconds_to_nsecs,
    should_drop_late_video_frame, stream_frame_duration_nsecs, timestamp_to_nsecs,
    wait_for_audio_clocked_video_queue,
};
use codec::{AudioResampler, AvFrame, AvPacket, Decoder, VideoScaler};
use dovi::{DoviMetadataQueue, dovi_metadata_from_frame};
#[cfg(test)]
use dovi::{dovi_packet_timeline_nsecs, has_annex_b_start_code};
use format::{FormatContext, StreamInfo};
use reporting::{BufferedReporter, PositionReporter};
use util::ffmpeg_error;
#[cfg(test)]
use video::ffmpeg_raw_video_format;
use video::{VideoFrameConverter, frame_size, video_frame_len};
use worker::{
    FfmpegCommand, FfmpegControl, FfmpegPlaybackInput, FfmpegWorker, drain_seek_command,
    ffmpeg_interrupt_callback,
};

const FALLBACK_AUDIO_OUTPUT_CHANNELS: c_int = 2;
const POSITION_QUERY_INTERVAL: Duration = Duration::from_millis(250);
const DEFAULT_VIDEO_FRAME_DURATION_NSECS: u64 = 1_000_000_000 / 24;
const SCHEDULER_POLL_INTERVAL: Duration = Duration::from_millis(5);
const RPU_MATCH_TOLERANCE: Duration = Duration::from_millis(60);
const RPU_QUEUE_CAPACITY: usize = 2048;
const AUDIO_BUFFER_SECONDS: usize = 4;
const AUDIO_VIDEO_QUEUE_LIMIT_DURATION: Duration = Duration::from_millis(300);
const AUDIO_VIDEO_QUEUE_TARGET_DURATION: Duration = Duration::from_millis(120);
const LATE_VIDEO_DROP_TOLERANCE: Duration = Duration::from_millis(75);
const HTTP_RING_CACHE_CAPACITY: usize = 500 * 1024 * 1024;
const HTTP_CACHE_CHUNK_SIZE: usize = 256 * 1024;
const HTTP_CACHE_RANGE_REQUEST_BYTES: u64 = 32 * 1024 * 1024;
const HTTP_CACHE_WAIT_INTERVAL: Duration = Duration::from_millis(50);
const HTTP_CACHE_CONTENT_LEN_WAIT: Duration = Duration::from_secs(1);
const HTTP_CACHE_RANGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES: u64 = 2 * 1024 * 1024;
const HTTP_CACHE_SMALL_RANGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_CACHE_PROGRESS_REPORT_THRESHOLD: f64 = 0.001;
const FFMPEG_AVIO_BUFFER_SIZE: c_int = 256 * 1024;
const FFMPEG_FAST_PROBE_SIZE: usize = 1024 * 1024;
const FFMPEG_FAST_ANALYZE_DURATION_US: u64 = 1_000_000;

static FFMPEG_FRAME_COUNT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputProbeProfile {
    Fast,
    Full,
}

pub struct FfmpegBackend {
    frame_slot: FrameSlot,
    event_tx: Sender<BackendEvent>,
    event_rx: Receiver<BackendEvent>,
    worker: Option<FfmpegWorker>,
    current_url: Option<String>,
    loaded: bool,
    paused: bool,
}

impl FfmpegBackend {
    pub fn new() -> Result<Self> {
        init_ffmpeg_network()?;

        let frame_slot = FrameSlot::default();
        let (event_tx, event_rx) = mpsc::channel();

        Ok(Self {
            frame_slot,
            event_tx,
            event_rx,
            worker: None,
            current_url: None,
            loaded: false,
            paused: true,
        })
    }

    pub fn frame_slot(&self) -> FrameSlot {
        self.frame_slot.clone()
    }

    pub fn load_url(
        &mut self,
        url: &str,
        http_headers: Vec<(String, String)>,
        content_length: Option<u64>,
    ) -> Result<()> {
        let url = url.trim();
        if url.is_empty() {
            return Err(BackendError::EmptyUrl);
        }

        self.stop_worker();
        self.frame_slot.clear();
        self.current_url = Some(url.to_string());
        self.loaded = false;
        self.paused = true;
        FFMPEG_FRAME_COUNT.store(0, Ordering::Relaxed);
        while self.event_rx.try_recv().is_ok() {}
        let _ = self
            .event_tx
            .send(BackendEvent::HttpStreamBufferedChanged(None));

        self.worker = Some(FfmpegWorker::spawn(
            FfmpegPlaybackInput {
                url: url.to_string(),
                http_headers,
                content_length,
                start_position_seconds: 0.0,
            },
            self.frame_slot.clone(),
            self.event_tx.clone(),
        )?);
        Ok(())
    }

    pub fn seek_to(&mut self, position_seconds: f64) -> Result<()> {
        let Some(worker) = self.worker.as_ref() else {
            return Err(BackendError::Ffmpeg(
                "FFmpeg 尚未加载可跳转的媒体".to_string(),
            ));
        };
        let position_seconds = position_seconds.max(0.0);

        self.frame_slot.clear();
        self.loaded = false;
        self.paused = true;
        FFMPEG_FRAME_COUNT.store(0, Ordering::Relaxed);
        while self.event_rx.try_recv().is_ok() {}
        worker.seek(position_seconds)?;
        let _ = self
            .event_tx
            .send(BackendEvent::PositionChanged(position_seconds));
        let _ = self
            .event_tx
            .send(BackendEvent::BufferedChanged(Some(position_seconds)));
        Ok(())
    }

    pub fn poll_events(&mut self) -> Vec<BackendEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            if matches!(event, BackendEvent::Pause(true)) {
                self.paused = true;
            }
            events.push(event);
        }

        if let Some(size) = self.frame_slot.take_size_change() {
            if !self.loaded {
                self.loaded = true;
                self.paused = false;
                events.push(BackendEvent::PlaybackRestart);
                events.push(BackendEvent::Pause(false));
                events.push(BackendEvent::Buffering(false));
            }
            events.push(BackendEvent::VideoSizeChanged(Some(size)));
        }

        events
    }

    fn stop_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.stop();
        }
    }
}

impl Drop for FfmpegBackend {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.stop_async();
        }
        self.frame_slot.clear();
    }
}

fn init_ffmpeg_network() -> Result<()> {
    static INITIALIZED: AtomicBool = AtomicBool::new(false);
    if INITIALIZED.load(Ordering::Relaxed) {
        return Ok(());
    }

    let result = unsafe { ffi::avformat_network_init() };
    if result < 0 {
        return Err(BackendError::Ffmpeg(format!(
            "初始化 FFmpeg 网络层失败：{}",
            ffmpeg_error(result)
        )));
    }
    INITIALIZED.store(true, Ordering::Relaxed);
    Ok(())
}

#[cfg(test)]
mod tests;
