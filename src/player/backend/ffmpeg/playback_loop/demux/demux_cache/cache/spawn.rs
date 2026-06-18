use std::{
    sync::{Arc, Condvar, Mutex, atomic::AtomicU64, mpsc::Sender},
    thread,
    time::Instant,
};

use super::{
    BackendEvent, DemuxPacketCache, DemuxPacketCacheInput, DemuxPacketCacheMonitorSnapshot,
    DemuxPacketCacheShared, DemuxPacketCacheState, DemuxPacketCacheThreadInput,
    DemuxSelectedStreams, FfmpegControl, run_demux_packet_cache, seconds_to_nsecs,
};

impl DemuxPacketCache {
    pub(in crate::player::backend::ffmpeg::playback_loop) fn spawn(
        cache_input: DemuxPacketCacheInput,
        control: Arc<FfmpegControl>,
        event_tx: Sender<BackendEvent>,
    ) -> std::result::Result<Self, String> {
        let DemuxPacketCacheInput {
            input,
            video_stream,
            audio_stream,
            subtitle_stream,
            duration_seconds,
            start_position_seconds,
            session_id,
            cache_config,
        } = cache_input;
        let start_position_seconds = start_position_seconds.max(0.0);
        let start_position_nsecs = seconds_to_nsecs(start_position_seconds);
        let mut state = DemuxPacketCacheState::new(
            start_position_nsecs,
            video_stream.index,
            video_stream.codec_id,
            session_id,
            cache_config,
        );
        state.set_selected_streams(DemuxSelectedStreams {
            audio_stream,
            subtitle_stream,
        });
        let monitor_snapshot = DemuxPacketCacheMonitorSnapshot::from_state(&state);
        let shared = Arc::new(DemuxPacketCacheShared {
            state: Mutex::new(state),
            monitor_snapshot: Mutex::new(monitor_snapshot),
            ready: Condvar::new(),
            control,
            event_tx,
            clock_start: Instant::now(),
            demux_read_started_nanos: AtomicU64::new(0),
            last_would_block_diag_nanos: AtomicU64::new(0),
        });
        shared.enter_initial_cache_pause_if_needed();
        let thread_shared = Arc::clone(&shared);
        let thread_input = DemuxPacketCacheThreadInput {
            input,
            video_stream,
            audio_stream,
            subtitle_stream,
            duration_seconds,
            start_position_seconds,
            session_id,
        };
        let handle = thread::Builder::new()
            .name("tiny-ffmpeg-demux-cache".to_string())
            .spawn(move || run_demux_packet_cache(thread_input, thread_shared))
            .map_err(|error| format!("创建 FFmpeg demux 缓存线程失败：{error}"))?;

        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }
}
