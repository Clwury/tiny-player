use std::{
    collections::VecDeque,
    env,
    ffi::{CStr, CString},
    fs::{File, OpenOptions},
    io::Read,
    mem,
    os::raw::{c_char, c_int, c_void},
    os::unix::fs::FileExt,
    path::PathBuf,
    ptr, slice,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cpal::{
    FromSample, Sample, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use ffmpeg_sys_next as ffi;

#[cfg(test)]
use super::PlaybackSeekableCacheMode;
use super::{
    BackendControl, BackendError, BackendEvent, BackendEventKind, BackendLoadRequest,
    BackendSubtitleBitmap, BackendSubtitleCue, ByteCacheState, CacheUnlinkPolicy, DemuxCacheState,
    PlaybackCacheByteRange, PlaybackCacheConfig, PlaybackCacheMode, PlaybackCacheState,
    PlaybackVideoInfo, Result, StreamCacheKind, StreamCacheState,
};
use crate::player::{
    dovi::{
        DoviFrameMetadata, DoviRpuExtractor, DoviRpuStripResult, HevcStreamFormat,
        strip_dovi_rpu_nalus,
    },
    ffmpeg_dovi::FfmpegDoviMetadata,
    render_host::{
        DecodedFrame, FfmpegAvBufferRef, FfmpegFrameRef, FrameBufferPool, FrameColor,
        FrameDynamicMetadata, FramePixels, FramePts, FrameSlot, PlaybackSessionId, PooledBytes,
        RawVideoChromaSite, RawVideoFormat, RawVideoFrame, RawVideoPlane, RawVideoPlanes,
        RawVideoRange, RenderSize, VulkanDecodeDevice, VulkanDecodeQueue, VulkanDecodeQueues,
        VulkanVideoFrame, VulkanVideoPlane, render_image_from_bgra,
    },
    tracks::PlaybackTrack,
};

mod audio;
mod avio;
mod bsf;
mod clock;
mod codec;
mod dovi;
mod format;
mod hw;
mod playback_loop;
mod reporting;
mod subtitle;
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
    CacheReadResult, HttpContentRange, HttpRingCacheState, content_len_from_content_range,
    content_range_from_headers, ffmpeg_http_headers, http_cache_range_header,
    http_cache_range_request_len, http_cache_range_request_timeout,
    http_cache_request_headers_for_log, http_cache_response_headers_for_log, should_cache_http_url,
};
use avio::{HttpRingCache, reqwest_header_pairs};
use bsf::PgsFrameMergeBitstreamFilter;
#[cfg(test)]
use clock::{
    MappedTimestamp, WaitStatus, pop_audio_clocked_video_frame,
    queued_video_frame_ready_for_audio_clock, should_drop_late_video_frame,
};
use clock::{
    PlaybackScheduler, QueuedVideoFrame, TimestampMapper, drain_audio_clocked_video_queue,
    duration_nsecs, frame_best_effort_timestamp, frame_decode_error_flags, frame_is_corrupt,
    max_optional_seconds, nsecs_to_seconds, nsecs_to_timestamp, optional_buffered_value_changed,
    present_decoded_video_frame, present_due_audio_clocked_video_frames, pts_distance,
    queued_video_duration, queued_video_limit_duration, queued_video_target_duration,
    seconds_to_nsecs, should_drop_late_queued_video_frame, stream_frame_duration_nsecs,
    timestamp_to_nsecs, wait_for_audio_clocked_video_queue,
};
use codec::{
    AudioResampler, AvFrame, AvPacket, Decoder, VideoScaler, packet_is_video_recovery_point,
    packet_is_video_seek_point,
};
use dovi::{DoviPipeline, ffmpeg_dovi_metadata_from_frame};
#[cfg(test)]
use dovi::{dovi_packet_timeline_nsecs, has_annex_b_start_code};
use format::{FormatContext, StreamInfo};
use hw::{
    HardwareDecodeMode, VideoHwDecodeContext, is_vulkan_frame, vulkan_frame_planes,
    vulkan_sw_format,
};
use reporting::{BufferedReporter, PositionReporter};
use subtitle::{
    DecodedSubtitleCue, decoded_subrip_packet_cue, decoded_subtitle_cues,
    load_external_subtitle_cues,
};
use util::ffmpeg_error;
#[cfg(test)]
use video::ffmpeg_raw_video_format;
use video::{VideoFrameConverter, frame_size, video_frame_len};
use worker::{
    FfmpegCommand, FfmpegControl, FfmpegPlaybackInput, FfmpegWorker, drain_playback_commands,
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
const AUDIO_CLOCK_VIDEO_PRESENT_LEAD: Duration = Duration::from_millis(15);
const AUDIO_OUTPUT_DELAY_LIMIT: Duration = Duration::from_millis(500);
const VULKAN_AUDIO_VIDEO_QUEUE_LIMIT_DURATION: Duration = Duration::from_millis(90);
const VULKAN_AUDIO_VIDEO_QUEUE_TARGET_DURATION: Duration = Duration::from_millis(40);
const PGS_SUBTITLE_VIDEO_QUEUE_LIMIT_DURATION: Duration = Duration::from_millis(700);
const PGS_SUBTITLE_VIDEO_QUEUE_TARGET_DURATION: Duration = Duration::from_millis(500);
#[cfg(test)]
const DEMUX_PACKET_CACHE_MEMORY_BYTES: usize = 256 * 1024 * 1024;
const DEMUX_PACKET_CACHE_WAIT_INTERVAL: Duration = Duration::from_millis(10);
const DEMUX_PACKET_CACHE_STALL_LOG_AFTER: Duration = Duration::from_millis(500);
const DEMUX_PACKET_CACHE_STALL_LOG_INTERVAL: Duration = Duration::from_secs(1);
const DEMUX_PACKET_CACHE_PREFETCH_PAUSE_LOG_AFTER: Duration = Duration::from_millis(500);
const DEMUX_PACKET_CACHE_PREFETCH_PAUSE_LOG_INTERVAL: Duration = Duration::from_secs(5);
const LATE_VIDEO_DROP_TOLERANCE: Duration = Duration::from_millis(75);
const DEFAULT_PLAYBACK_VOLUME: f32 = 1.0;
const PLAYBACK_VOLUME_SCALE: u32 = 10_000;
#[cfg(test)]
const HTTP_RING_CACHE_CAPACITY: usize = 500 * 1024 * 1024;
const HTTP_CACHE_CHUNK_SIZE: usize = 1024 * 1024;
#[cfg(test)]
const HTTP_CACHE_RANGE_REQUEST_BYTES: u64 = 32 * 1024 * 1024;
const HTTP_CACHE_SIDE_DOWNLOAD_WORKERS: usize = 2;
const HTTP_CACHE_WAIT_INTERVAL: Duration = Duration::from_millis(50);
const HTTP_CACHE_STALL_LOG_AFTER: Duration = Duration::from_millis(500);
const HTTP_CACHE_STALL_LOG_INTERVAL: Duration = Duration::from_secs(1);
const HTTP_CACHE_PREFETCH_PAUSE_LOG_AFTER: Duration = Duration::from_millis(500);
const HTTP_CACHE_PREFETCH_PAUSE_LOG_INTERVAL: Duration = Duration::from_secs(5);
const HTTP_CACHE_PARTIAL_READ_MIN_BYTES: usize = 256 * 1024;
const HTTP_CACHE_CONTENT_LEN_WAIT: Duration = Duration::from_secs(1);
const HTTP_CACHE_RANGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_CACHE_SMALL_RANGE_REQUEST_BYTES: u64 = 2 * 1024 * 1024;
const HTTP_CACHE_SMALL_RANGE_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_CACHE_PROGRESS_REPORT_THRESHOLD: f64 = 0.001;
#[cfg(test)]
const HTTP_CACHE_PROBE_READ_WAIT: Duration = Duration::from_millis(250);
#[cfg(test)]
const HTTP_CACHE_DEFAULT_READAHEAD_SECONDS: f64 = 120.0;
#[cfg(test)]
const HTTP_CACHE_DEFAULT_HYSTERESIS_SECONDS: f64 = 10.0;
const FFMPEG_AVIO_BUFFER_SIZE: c_int = 1024 * 1024;
const FFMPEG_FAST_PROBE_SIZE: usize = 1024 * 1024;
const FFMPEG_FAST_ANALYZE_DURATION_US: u64 = 1_000_000;
const FFMPEG_SUBTITLE_PROBE_SIZE: usize = 64 * 1024 * 1024;
const FFMPEG_SUBTITLE_ANALYZE_DURATION_US: u64 = 30_000_000;

static FFMPEG_FRAME_COUNT: AtomicU64 = AtomicU64::new(0);

fn normalize_playback_volume(volume: f32) -> f32 {
    let volume = if volume.is_finite() {
        volume
    } else {
        DEFAULT_PLAYBACK_VOLUME
    };
    volume.clamp(0.0, 1.0)
}

fn valid_backend_seconds(seconds: f64) -> Option<f64> {
    (seconds.is_finite() && seconds >= 0.0).then_some(seconds)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputProbeProfile {
    Fast,
    Subtitle,
    Full,
}

pub struct FfmpegBackend {
    frame_slot: FrameSlot,
    event_tx: Sender<BackendEvent>,
    event_rx: Receiver<BackendEvent>,
    worker: Option<FfmpegWorker>,
    current_url: Option<String>,
    current_request: Option<BackendLoadRequest>,
    current_session_id: PlaybackSessionId,
    loaded: bool,
    user_paused: bool,
    paused: bool,
    volume: f32,
    cache_config: PlaybackCacheConfig,
    cache_state: PlaybackCacheState,
    position_seconds: Option<f64>,
    duration_seconds: Option<f64>,
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
            current_request: None,
            current_session_id: PlaybackSessionId::default(),
            loaded: false,
            user_paused: true,
            paused: true,
            volume: DEFAULT_PLAYBACK_VOLUME,
            cache_config: PlaybackCacheConfig::default(),
            cache_state: PlaybackCacheState::default(),
            position_seconds: None,
            duration_seconds: None,
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
        selected_tracks: crate::player::PlaybackTrackSelection,
    ) -> Result<()> {
        let url = url.trim();
        if url.is_empty() {
            return Err(BackendError::EmptyUrl);
        }

        let request = BackendLoadRequest {
            url: url.to_string(),
            http_headers,
            content_length,
            selected_tracks,
            cache_config: self.cache_config.clone(),
        };
        self.start_playback(request, 0.0)
    }

    fn start_playback(
        &mut self,
        mut request: BackendLoadRequest,
        start_position_seconds: f64,
    ) -> Result<()> {
        request.cache_config = request.cache_config.clone().normalized();
        let session_id = self.advance_session();
        self.frame_slot.begin_session(session_id);
        self.stop_worker();
        self.current_url = Some(request.url.clone());
        self.current_request = Some(request.clone());
        self.loaded = false;
        self.user_paused = false;
        self.paused = true;
        self.cache_config = request.cache_config.clone();
        self.cache_state = PlaybackCacheState::default();
        self.position_seconds = Some(start_position_seconds.max(0.0));
        self.duration_seconds = None;
        FFMPEG_FRAME_COUNT.store(0, Ordering::Relaxed);
        while self.event_rx.try_recv().is_ok() {}
        let _ = self.event_tx.send(BackendEvent::new(
            session_id,
            BackendEventKind::CacheStateChanged(self.cache_state.clone()),
        ));

        self.worker = Some(FfmpegWorker::spawn(
            FfmpegPlaybackInput {
                session_id,
                url: request.url,
                http_headers: request.http_headers,
                content_length: request.content_length,
                start_position_seconds,
                selected_tracks: request.selected_tracks,
                cache_config: request.cache_config,
            },
            self.frame_slot.clone(),
            self.event_tx.clone(),
            self.volume,
        )?);
        Ok(())
    }

    fn set_track_selection_in_place(
        &mut self,
        selected_tracks: crate::player::PlaybackTrackSelection,
        position_seconds: f64,
    ) -> Result<()> {
        if self.worker.is_none() {
            return Err(BackendError::Ffmpeg(
                "FFmpeg 尚未加载可切换轨道的媒体".to_string(),
            ));
        }

        let mut request = self
            .current_request
            .clone()
            .ok_or_else(|| BackendError::Ffmpeg("FFmpeg 尚未加载可切换轨道的媒体".to_string()))?;
        request.selected_tracks = selected_tracks.clone();
        let position_seconds = position_seconds.max(0.0);
        let pause_after_switch = self.user_paused;
        let session_id = self.advance_session();
        self.frame_slot.begin_session(session_id);
        if !pause_after_switch {
            self.loaded = false;
        }
        self.paused = true;
        FFMPEG_FRAME_COUNT.store(0, Ordering::Relaxed);
        while self.event_rx.try_recv().is_ok() {}

        let worker = self
            .worker
            .as_ref()
            .expect("worker exists after early return");
        worker.set_track_selection(
            selected_tracks,
            position_seconds,
            session_id,
            pause_after_switch,
        )?;
        self.current_request = Some(request);
        let _ = self.event_tx.send(BackendEvent::new(
            session_id,
            BackendEventKind::PositionChanged(position_seconds),
        ));
        let _ = self.event_tx.send(BackendEvent::new(
            session_id,
            BackendEventKind::PausedForCacheChanged(false),
        ));
        let _ = self.event_tx.send(BackendEvent::new(
            session_id,
            BackendEventKind::CacheBufferingChanged(None),
        ));
        Ok(())
    }

    pub fn seek_to(&mut self, position_seconds: f64) -> Result<()> {
        if self.worker.is_none() {
            return Err(BackendError::Ffmpeg(
                "FFmpeg 尚未加载可跳转的媒体".to_string(),
            ));
        }
        let position_seconds = position_seconds.max(0.0);

        let session_id = self.advance_session();
        self.frame_slot.begin_session(session_id);
        self.loaded = false;
        self.paused = true;
        FFMPEG_FRAME_COUNT.store(0, Ordering::Relaxed);
        while self.event_rx.try_recv().is_ok() {}
        let worker = self
            .worker
            .as_ref()
            .expect("worker exists after early return");
        worker.seek(position_seconds, session_id)?;
        let _ = self.event_tx.send(BackendEvent::new(
            session_id,
            BackendEventKind::PositionChanged(position_seconds),
        ));
        let _ = self.event_tx.send(BackendEvent::new(
            session_id,
            BackendEventKind::PausedForCacheChanged(false),
        ));
        let _ = self.event_tx.send(BackendEvent::new(
            session_id,
            BackendEventKind::CacheBufferingChanged(None),
        ));
        Ok(())
    }

    pub fn set_paused(&mut self, paused: bool) -> Result<()> {
        let Some(worker) = self.worker.as_ref() else {
            return Err(BackendError::Ffmpeg(
                "FFmpeg 尚未加载可控制的媒体".to_string(),
            ));
        };
        worker.set_paused(paused, self.current_session_id)?;
        self.user_paused = paused;
        let effective_paused = worker.is_paused();
        self.paused = effective_paused;
        let _ = self.event_tx.send(BackendEvent::new(
            self.current_session_id,
            BackendEventKind::Pause(effective_paused),
        ));
        Ok(())
    }

    pub fn pause(&mut self) -> Result<()> {
        self.set_paused(true)
    }

    pub fn resume(&mut self) -> Result<()> {
        self.set_paused(false)
    }

    pub fn set_volume(&mut self, volume: f32) -> Result<()> {
        let volume = normalize_playback_volume(volume);
        self.volume = volume;
        if let Some(worker) = self.worker.as_ref() {
            worker.set_volume(volume);
        }
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        let session_id = self.advance_session();
        self.frame_slot.begin_session(session_id);
        self.stop_worker();
        self.current_url = None;
        self.current_request = None;
        self.loaded = false;
        self.user_paused = true;
        self.paused = true;
        self.cache_state = PlaybackCacheState::default();
        self.position_seconds = None;
        self.duration_seconds = None;
        while self.event_rx.try_recv().is_ok() {}
        let _ = self
            .event_tx
            .send(BackendEvent::new(session_id, BackendEventKind::Pause(true)));
        Ok(())
    }

    pub fn poll_events(&mut self) -> Vec<BackendEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            if event.session_id != self.current_session_id {
                continue;
            }
            let forward_original = !matches!(&event.kind, BackendEventKind::CacheStateChanged(_));
            let cache_update = self.cache_update_for_event(&event);
            match &event.kind {
                BackendEventKind::Pause(paused) => {
                    if !self.cache_state.paused_for_cache {
                        self.user_paused = *paused;
                    }
                    self.paused = self.user_paused || self.cache_state.paused_for_cache;
                }
                BackendEventKind::PausedForCacheChanged(paused_for_cache) => {
                    self.paused = self.user_paused || *paused_for_cache;
                }
                BackendEventKind::PlaybackEnded => {
                    self.paused = true;
                }
                _ => {}
            }
            if forward_original {
                events.push(event);
            }
            if let Some(cache_state) = cache_update {
                events.push(BackendEvent::new(
                    self.current_session_id,
                    BackendEventKind::CacheStateChanged(cache_state),
                ));
            }
        }

        if let Some((session_id, size)) = self.frame_slot.take_size_change() {
            if session_id != self.current_session_id {
                return events;
            }
            if !self.loaded {
                self.loaded = true;
                let effective_paused = self
                    .worker
                    .as_ref()
                    .map(|worker| worker.is_paused())
                    .unwrap_or(self.paused);
                self.paused = effective_paused;
                events.push(BackendEvent::new(
                    session_id,
                    BackendEventKind::PlaybackRestart,
                ));
                events.push(BackendEvent::new(
                    session_id,
                    BackendEventKind::Pause(effective_paused),
                ));
                events.push(BackendEvent::new(
                    session_id,
                    BackendEventKind::Buffering(false),
                ));
            }
            events.push(BackendEvent::new(
                session_id,
                BackendEventKind::VideoSizeChanged(Some(size)),
            ));
        }

        events
    }

    pub fn set_cache_config(&mut self, config: PlaybackCacheConfig) -> Result<()> {
        self.cache_config = config.normalized();
        if let Some(request) = self.current_request.as_mut() {
            request.cache_config = self.cache_config.clone();
        }
        if let Some(worker) = &self.worker {
            worker.set_cache_config(self.current_session_id, self.cache_config.clone())?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn cache_state(&self) -> Option<PlaybackCacheState> {
        Some(self.cache_state.clone())
    }

    fn cache_update_for_event(&mut self, event: &BackendEvent) -> Option<PlaybackCacheState> {
        match &event.kind {
            BackendEventKind::PositionChanged(position) => {
                self.position_seconds = valid_backend_seconds(*position);
                None
            }
            BackendEventKind::DurationChanged(duration) => {
                self.duration_seconds = valid_backend_seconds(*duration);
                None
            }
            BackendEventKind::BufferedChanged(_) => None,
            BackendEventKind::CacheStateChanged(state) => {
                if let Some(byte_state) = state.byte.as_ref()
                    && !demux_state_carries_demux_cache_data(&state.demux)
                {
                    self.cache_state.byte = state.byte.clone();
                    self.cache_state.demux.raw_input_rate = byte_state.raw_input_rate;
                    self.cache_state.demux.byte_level_seeks = self
                        .cache_state
                        .demux
                        .byte_level_seeks
                        .max(byte_state.byte_level_seeks);
                    return Some(self.cache_state.clone());
                }

                let mut state = state.clone();
                if state.byte.is_none() {
                    state.byte = self.cache_state.byte.clone();
                }
                if let Some(byte_state) = state.byte.as_ref() {
                    state.demux.raw_input_rate = byte_state.raw_input_rate;
                    state.demux.byte_level_seeks = self
                        .cache_state
                        .demux
                        .byte_level_seeks
                        .max(byte_state.byte_level_seeks);
                } else {
                    if state.demux.raw_input_rate.is_none() {
                        state.demux.raw_input_rate = self.cache_state.demux.raw_input_rate;
                    }
                    if state.demux.byte_level_seeks == 0 {
                        state.demux.byte_level_seeks = self.cache_state.demux.byte_level_seeks;
                    }
                }
                self.cache_state = state;
                Some(self.cache_state.clone())
            }
            BackendEventKind::PausedForCacheChanged(paused_for_cache) => {
                self.cache_state.paused_for_cache = *paused_for_cache;
                if !paused_for_cache {
                    self.cache_state.buffering_percent = None;
                }
                Some(self.cache_state.clone())
            }
            BackendEventKind::CacheBufferingChanged(percent) => {
                self.cache_state.buffering_percent = *percent;
                Some(self.cache_state.clone())
            }
            BackendEventKind::PlaybackEnded => {
                self.cache_state.demux.eof = true;
                self.cache_state.demux.idle = true;
                self.cache_state.paused_for_cache = false;
                self.cache_state.buffering_percent = None;
                Some(self.cache_state.clone())
            }
            BackendEventKind::LoadFailed(_) | BackendEventKind::Fatal(_) => {
                self.cache_state = PlaybackCacheState::default();
                self.user_paused = true;
                self.position_seconds = None;
                self.duration_seconds = None;
                Some(self.cache_state.clone())
            }
            _ => None,
        }
    }

    fn advance_session(&mut self) -> PlaybackSessionId {
        self.current_session_id = self.current_session_id.next();
        self.current_session_id
    }

    fn stop_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.stop();
        }
    }
}

fn demux_state_carries_demux_cache_data(state: &DemuxCacheState) -> bool {
    state.cache_end.is_some()
        || state.reader_pts.is_some()
        || state.cache_duration.is_some()
        || state.eof
        || state.underrun
        || state.idle
        || state.seeking
        || state.bof_cached
        || state.eof_cached
        || state.total_bytes > 0
        || state.forward_bytes > 0
        || state.file_cache_bytes.is_some()
        || state.cached_seeks > 0
        || state.low_level_seeks > 0
        || state.ts_last.is_some()
        || !state.seekable_ranges.is_empty()
        || !state.streams.is_empty()
}

impl BackendControl for FfmpegBackend {
    fn load(&mut self, request: BackendLoadRequest) -> Result<()> {
        let BackendLoadRequest {
            url,
            http_headers,
            content_length,
            selected_tracks,
            cache_config,
        } = request;
        self.cache_config = cache_config;
        self.load_url(&url, http_headers, content_length, selected_tracks)
    }

    fn seek(&mut self, position_seconds: f64) -> Result<()> {
        FfmpegBackend::seek_to(self, position_seconds)
    }

    fn pause(&mut self) -> Result<()> {
        FfmpegBackend::pause(self)
    }

    fn resume(&mut self) -> Result<()> {
        FfmpegBackend::resume(self)
    }

    fn stop(&mut self) -> Result<()> {
        FfmpegBackend::stop(self)
    }

    fn set_audio_track(&mut self, track_index: Option<usize>, position_seconds: f64) -> Result<()> {
        let mut selected_tracks = self
            .current_request
            .as_ref()
            .map(|request| request.selected_tracks.clone())
            .unwrap_or_default();
        selected_tracks.audio_stream_index = track_index;
        self.set_track_selection_in_place(selected_tracks, position_seconds)
    }

    fn set_subtitle_track(
        &mut self,
        track: Option<PlaybackTrack>,
        position_seconds: f64,
    ) -> Result<()> {
        let mut selected_tracks = self
            .current_request
            .as_ref()
            .map(|request| request.selected_tracks.clone())
            .unwrap_or_default();
        selected_tracks.set_subtitle_track(track.as_ref());
        self.set_track_selection_in_place(selected_tracks, position_seconds)
    }

    fn set_volume(&mut self, volume: f32) -> Result<()> {
        FfmpegBackend::set_volume(self, volume)
    }

    fn set_cache_config(&mut self, config: PlaybackCacheConfig) -> Result<()> {
        FfmpegBackend::set_cache_config(self, config)
    }

    fn cache_state(&self) -> Option<PlaybackCacheState> {
        FfmpegBackend::cache_state(self)
    }

    fn poll_events(&mut self) -> Vec<BackendEvent> {
        FfmpegBackend::poll_events(self)
    }

    fn frame_slot(&self) -> FrameSlot {
        FfmpegBackend::frame_slot(self)
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
