use super::{DemuxPacketCacheState, DemuxReaderWatermark, StreamCacheKind, StreamForwardWindow};

impl DemuxPacketCacheState {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn should_pause_demux(
        &self,
    ) -> bool {
        if self.demux_position_detached {
            return true;
        }
        if self.selected_eager_stream_needs_packet() {
            return false;
        }
        if self.stream_packet_queue_full() {
            return true;
        }
        if self.memory_limit_bytes > 0 && self.forward_bytes() >= self.memory_limit_bytes {
            return true;
        }
        let forward_duration = self.forward_duration_nsecs();
        if forward_duration >= self.readahead_nsecs {
            return true;
        }
        let resume_threshold = self.readahead_nsecs.saturating_sub(self.hysteresis_nsecs);
        self.hysteresis_active && self.hysteresis_nsecs > 0 && forward_duration > resume_threshold
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn reader_watermark(
        &self,
    ) -> DemuxReaderWatermark {
        let mut video_forward_nsecs = None;
        let mut audio_forward_nsecs = None;
        let mut selected_min_forward_nsecs = None;
        let mut video_seen = false;
        let mut audio_seen = false;
        let mut video_underrun = false;
        let mut audio_underrun = false;
        let mut video_idle = false;
        let mut audio_idle = false;
        let reader_windows = self.reader_stream_forward_windows();
        for window in reader_windows.iter().copied() {
            let duration_nsecs = window.duration_nsecs();
            let stream_idle = self.stream_window_idle(window);
            let stream_underrun = self.stream_window_underrun(window);
            match window.kind {
                StreamCacheKind::Video => {
                    video_underrun |= stream_underrun;
                    video_idle = if video_seen {
                        video_idle && stream_idle
                    } else {
                        stream_idle
                    };
                    video_seen = true;
                    video_forward_nsecs = Some(
                        video_forward_nsecs
                            .unwrap_or(duration_nsecs)
                            .min(duration_nsecs),
                    );
                    selected_min_forward_nsecs = Some(
                        selected_min_forward_nsecs
                            .unwrap_or(duration_nsecs)
                            .min(duration_nsecs),
                    );
                }
                StreamCacheKind::Audio => {
                    audio_underrun |= stream_underrun;
                    audio_idle = if audio_seen {
                        audio_idle && stream_idle
                    } else {
                        stream_idle
                    };
                    audio_seen = true;
                    audio_forward_nsecs = Some(
                        audio_forward_nsecs
                            .unwrap_or(duration_nsecs)
                            .min(duration_nsecs),
                    );
                    selected_min_forward_nsecs = Some(
                        selected_min_forward_nsecs
                            .unwrap_or(duration_nsecs)
                            .min(duration_nsecs),
                    );
                }
                StreamCacheKind::Subtitle | StreamCacheKind::Unknown => {}
            }
        }
        DemuxReaderWatermark {
            video_forward_nsecs,
            audio_forward_nsecs,
            selected_min_forward_nsecs,
            video_underrun,
            audio_underrun,
            video_idle: video_seen && video_idle,
            audio_idle: audio_seen && audio_idle,
            underrun: reader_windows
                .into_iter()
                .any(|window| self.stream_window_underrun(window)),
            idle: self.effective_eof()
                || (!video_underrun
                    && !audio_underrun
                    && selected_min_forward_nsecs.is_some()
                    && self.should_pause_demux()),
            forward_bytes: u64::try_from(self.reader_forward_bytes()).unwrap_or(u64::MAX),
        }
    }

    fn selected_eager_stream_needs_packet(&self) -> bool {
        self.active_stream_forward_windows()
            .into_iter()
            .any(|window| self.stream_window_needs_reader_packet(window))
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn initial_cache_fill_complete(
        &self,
    ) -> bool {
        if self.cache_pause_enabled && self.cache_pause_initial && self.cache_pause_wait_nsecs > 0 {
            self.cache_pause_recovered()
        } else {
            self.effective_eof() || self.should_pause_demux()
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn refresh_readahead_hysteresis(
        &mut self,
    ) {
        if self.hysteresis_nsecs == 0 {
            self.hysteresis_active = false;
            return;
        }
        let forward_duration = self.forward_duration_nsecs();
        let resume_threshold = self.readahead_nsecs.saturating_sub(self.hysteresis_nsecs);
        if self.hysteresis_active {
            if forward_duration <= resume_threshold {
                self.hysteresis_active = false;
            }
        } else if forward_duration >= self.readahead_nsecs {
            self.hysteresis_active = true;
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn cached_until_nsecs(
        &self,
    ) -> Option<u64> {
        let active_end = self
            .cached_timeline_range()
            .map(|(_, buffered_until_nsecs)| buffered_until_nsecs);
        let detached_end = self.detached_append_range().and_then(|range| {
            Self::cached_timeline_range_in_packet_range(
                &self.packets,
                self.timeline_anchor_stream_index,
                &range.stream_queues,
            )
            .map(|(_, buffered_until_nsecs)| buffered_until_nsecs)
        });
        active_end.into_iter().chain(detached_end).max()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn forward_duration_nsecs(
        &self,
    ) -> u64 {
        self.selected_forward_timeline_window()
            .map(StreamForwardWindow::duration_nsecs)
            .unwrap_or_else(|| {
                self.cached_until_nsecs()
                    .map(|cached_until| cached_until.saturating_sub(self.reader_nsecs))
                    .unwrap_or_default()
            })
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn cache_pause_percent(
        &self,
    ) -> Option<u8> {
        if self.cache_pause_wait_nsecs == 0 {
            return None;
        }
        let percent =
            self.forward_duration_nsecs().saturating_mul(100) / self.cache_pause_wait_nsecs;
        Some(u8::try_from(percent.min(99)).unwrap_or(99))
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn cache_pause_can_enter(
        &self,
        require_demux_underrun: bool,
    ) -> bool {
        self.cache_pause_enabled
            && self.cache_pause_wait_nsecs > 0
            && !self.effective_eof()
            && !self.cache_pause_recovered()
            && (!require_demux_underrun || self.has_demux_underrun())
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn cache_pause_recovered(
        &self,
    ) -> bool {
        self.effective_eof()
            || self.forward_duration_nsecs() >= self.cache_pause_wait_nsecs
            || self.should_pause_demux()
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn mark_eof(&mut self) {
        self.seeking = false;
        if let Some(range) = self.detached_append_range_mut() {
            range.is_eof = true;
        } else {
            self.read_range_mut().is_eof = true;
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn effective_eof(
        &self,
    ) -> bool {
        self.read_range_eof()
            || self.detached_append_range().is_some_and(|range| {
                range.is_eof && self.read_index >= self.read_range().global_order.len()
            })
    }
}
