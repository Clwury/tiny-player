use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender},
};

use ffmpeg_sys_next as ffi;

use crate::player::{
    backend::{
        BackendControl, BackendError, BackendEvent, BackendEventKind, BackendLoadRequest,
        DemuxCacheState, PlaybackCacheConfig, PlaybackCacheState, PlaybackSeekMode, Result,
    },
    render_host::{PlaybackSessionId, VideoOutputQueue},
    tracks::PlaybackTrack,
};

use super::{
    DEFAULT_PLAYBACK_VOLUME, FFMPEG_FRAME_COUNT, FfmpegPlaybackInput, FfmpegWorker, ffmpeg_error,
    normalize_playback_volume,
};

fn valid_backend_seconds(seconds: f64) -> Option<f64> {
    (seconds.is_finite() && seconds >= 0.0).then_some(seconds)
}

pub struct FfmpegBackend {
    pub(super) video_output_queue: VideoOutputQueue,
    pub(super) event_tx: Sender<BackendEvent>,
    pub(super) event_rx: Receiver<BackendEvent>,
    pub(super) worker: Option<FfmpegWorker>,
    pub(super) current_url: Option<String>,
    pub(super) current_request: Option<BackendLoadRequest>,
    pub(super) current_session_id: PlaybackSessionId,
    pub(super) loaded: bool,
    pub(super) user_paused: bool,
    pub(super) paused: bool,
    pub(super) volume: f32,
    pub(super) cache_config: PlaybackCacheConfig,
    pub(super) cache_state: PlaybackCacheState,
    pub(super) position_seconds: Option<f64>,
    pub(super) duration_seconds: Option<f64>,
}

impl FfmpegBackend {
    pub fn new() -> Result<Self> {
        init_ffmpeg_network()?;

        let video_output_queue = VideoOutputQueue::default();
        let (event_tx, event_rx) = mpsc::channel();

        Ok(Self {
            video_output_queue,
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

    pub fn video_output_queue(&self) -> VideoOutputQueue {
        self.video_output_queue.clone()
    }

    #[allow(dead_code)]
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
            start_position_seconds: 0.0,
            selected_tracks,
            cache_config: self.cache_config.clone(),
        };
        self.start_playback(request)
    }

    fn start_playback(&mut self, mut request: BackendLoadRequest) -> Result<()> {
        let start_position_seconds = request.start_position_seconds.max(0.0);
        request.cache_config = request.cache_config.clone().normalized();
        let session_id = self.advance_session();
        self.video_output_queue.begin_session(session_id);
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
            self.video_output_queue.clone(),
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
        self.video_output_queue.begin_session(session_id);
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
        self.seek_to_with_mode(position_seconds, PlaybackSeekMode::Precise)
    }

    pub fn seek_to_with_mode(
        &mut self,
        position_seconds: f64,
        seek_mode: PlaybackSeekMode,
    ) -> Result<()> {
        if self.worker.is_none() {
            return Err(BackendError::Ffmpeg(
                "FFmpeg 尚未加载可跳转的媒体".to_string(),
            ));
        }
        let position_seconds = position_seconds.max(0.0);

        let session_id = self.advance_session();
        self.video_output_queue.begin_session(session_id);
        self.loaded = false;
        self.paused = true;
        FFMPEG_FRAME_COUNT.store(0, Ordering::Relaxed);
        while self.event_rx.try_recv().is_ok() {}
        let worker = self
            .worker
            .as_ref()
            .expect("worker exists after early return");
        worker.seek(position_seconds, seek_mode, session_id)?;
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
        self.video_output_queue.begin_session(session_id);
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

        if let Some((session_id, size)) = self.video_output_queue.take_size_change() {
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
            start_position_seconds,
            selected_tracks,
            cache_config,
        } = request;
        let url = url.trim();
        if url.is_empty() {
            return Err(BackendError::EmptyUrl);
        }
        self.cache_config = cache_config;
        self.start_playback(BackendLoadRequest {
            url: url.to_string(),
            http_headers,
            content_length,
            start_position_seconds,
            selected_tracks,
            cache_config: self.cache_config.clone(),
        })
    }

    fn seek(&mut self, position_seconds: f64) -> Result<()> {
        FfmpegBackend::seek_to(self, position_seconds)
    }

    fn seek_with_mode(&mut self, position_seconds: f64, mode: PlaybackSeekMode) -> Result<()> {
        FfmpegBackend::seek_to_with_mode(self, position_seconds, mode)
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

    fn video_output_queue(&self) -> VideoOutputQueue {
        FfmpegBackend::video_output_queue(self)
    }
}

impl Drop for FfmpegBackend {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.stop_async();
        }
        self.video_output_queue.clear();
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
