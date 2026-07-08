use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    os::raw::c_int,
};

use ffmpeg_sys_next as ffi;

use super::{
    DemuxCachedRange, DemuxPacketCacheState, DemuxPacketDiskCache, DemuxSelectedStreams,
    PlaybackCacheConfig, PlaybackCacheMode, PlaybackSessionId, StreamCacheKind,
    demux_packet_cache_hysteresis_nsecs, demux_packet_cache_readahead_nsecs,
    demux_packet_disk_cache_enabled, seconds_to_nsecs, video_cached_seek_preroll_nsecs,
};

impl DemuxPacketCacheState {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn new(
        reader_nsecs: u64,
        timeline_anchor_stream_index: c_int,
        timeline_anchor_codec_id: ffi::AVCodecID,
        session_id: PlaybackSessionId,
        cache_config: PlaybackCacheConfig,
    ) -> Self {
        let cache_config = cache_config.normalized();
        let disk_cache = DemuxPacketDiskCache::from_config(&cache_config);
        let disk_cache_writable = disk_cache.is_some();
        let memory_limit_bytes =
            usize::try_from(cache_config.demuxer_max_bytes).unwrap_or(usize::MAX);
        let cache_active = !matches!(cache_config.mode, PlaybackCacheMode::Disabled);
        let seekable_cache_active = cache_config.seekable_cache_active(cache_active);
        let backbuffer_limit_bytes = if seekable_cache_active {
            usize::try_from(cache_config.demuxer_max_back_bytes).unwrap_or(usize::MAX)
        } else {
            0
        };
        let readahead_nsecs = demux_packet_cache_readahead_nsecs(&cache_config, cache_active);
        let hysteresis_nsecs = demux_packet_cache_hysteresis_nsecs(&cache_config, readahead_nsecs);
        let cache_pause_wait_nsecs = seconds_to_nsecs(cache_config.cache_pause_wait);
        let mut stream_kinds = BTreeMap::new();
        stream_kinds.insert(timeline_anchor_stream_index, StreamCacheKind::Video);
        let mut ranges = BTreeMap::new();
        ranges.insert(0, DemuxCachedRange::new(0, reader_nsecs == 0, 0));
        Self {
            packets: HashMap::new(),
            ranges,
            disk_cache,
            disk_cache_writable,
            read_index: 0,
            consumed_packet_ids: HashSet::new(),
            reader_heads: BTreeMap::new(),
            reader_head_positions: BTreeMap::new(),
            reader_head_generations: BTreeMap::new(),
            forward_streams: BTreeMap::new(),
            reader_forward_bytes: 0,
            read_range_id: 0,
            append_range_id: 0,
            next_range_id: 1,
            next_packet_id: 0,
            timeline_anchor_stream_index,
            stream_kinds,
            selected_streams: DemuxSelectedStreams::default(),
            cached_seek_preroll_nsecs: video_cached_seek_preroll_nsecs(timeline_anchor_codec_id),
            cached_seek_requires_safe_point: timeline_anchor_codec_id
                == ffi::AVCodecID::AV_CODEC_ID_HEVC,
            memory_limit_bytes,
            backbuffer_limit_bytes,
            donate_backbuffer: cache_config.demuxer_donate_buffer,
            readahead_nsecs,
            hysteresis_nsecs,
            hysteresis_active: false,
            cache_pause_enabled: cache_active && cache_config.cache_pause,
            cache_pause_initial: cache_config.cache_pause_initial,
            cache_pause_wait_nsecs,
            cache_buffering_percent: None,
            cached_bytes: 0,
            append_maintenance_packets: 0,
            read_trim_pressure_packets: 0,
            reader_nsecs,
            session_id,
            seek_request: None,
            demux_position_detached: false,
            resume_append_skip_until_nsecs: None,
            low_level_append_guard_target_nsecs: None,
            low_level_append_blocked_packet_generations: HashMap::new(),
            seeking: false,
            demux_ts_nsecs: None,
            cached_seeks: 0,
            low_level_seeks: 0,
            input_rate_samples: VecDeque::new(),
            last_reported_buffered_until: None,
            last_cache_state_emit_at: None,
            last_emitted_seekable_ranges: None,
            cache_state_emit_dirty: false,
            generation: 0,
            error: None,
            shutdown: false,
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn set_stream_kind(
        &mut self,
        stream_index: c_int,
        kind: StreamCacheKind,
    ) {
        self.stream_kinds.insert(stream_index, kind);
        let range_ids = self.ranges.keys().copied().collect::<Vec<_>>();
        for range_id in range_ids {
            self.refresh_range_stream_seek_boundary(range_id, stream_index);
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn set_selected_streams(
        &mut self,
        selected_streams: DemuxSelectedStreams,
    ) {
        self.selected_streams = selected_streams;
        self.stream_kinds
            .retain(|_, kind| !matches!(kind, StreamCacheKind::Audio | StreamCacheKind::Subtitle));
        if let Some(audio_stream) = selected_streams.audio_stream {
            self.set_stream_kind(audio_stream.index, StreamCacheKind::Audio);
        }
        if let Some(subtitle_stream) = selected_streams.subtitle_stream {
            self.set_stream_kind(subtitle_stream.index, StreamCacheKind::Subtitle);
        }
        self.reader_heads
            .retain(|stream_index, _| self.stream_kinds.contains_key(stream_index));
        self.reader_head_positions
            .retain(|stream_index, _| self.stream_kinds.contains_key(stream_index));
        self.reader_head_generations
            .retain(|stream_index, _| self.stream_kinds.contains_key(stream_index));
        self.mark_all_seekable_summaries_dirty();
        self.refresh_reader_tracking();
        self.refresh_readahead_hysteresis();
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn apply_cache_config(
        &mut self,
        cache_config: PlaybackCacheConfig,
    ) {
        let cache_config = cache_config.normalized();
        let cache_active = !matches!(cache_config.mode, PlaybackCacheMode::Disabled);
        let seekable_cache_active = cache_config.seekable_cache_active(cache_active);

        self.memory_limit_bytes =
            usize::try_from(cache_config.demuxer_max_bytes).unwrap_or(usize::MAX);
        self.backbuffer_limit_bytes = if seekable_cache_active {
            usize::try_from(cache_config.demuxer_max_back_bytes).unwrap_or(usize::MAX)
        } else {
            0
        };
        self.donate_backbuffer = cache_config.demuxer_donate_buffer;
        self.readahead_nsecs = demux_packet_cache_readahead_nsecs(&cache_config, cache_active);
        self.hysteresis_nsecs =
            demux_packet_cache_hysteresis_nsecs(&cache_config, self.readahead_nsecs);
        if self.hysteresis_nsecs == 0 {
            self.hysteresis_active = false;
        }
        self.cache_pause_enabled = cache_active && cache_config.cache_pause;
        self.cache_pause_initial = cache_config.cache_pause_initial;
        self.cache_pause_wait_nsecs = seconds_to_nsecs(cache_config.cache_pause_wait);
        if !self.cache_pause_enabled {
            self.cache_buffering_percent = None;
        }

        let disk_cache_requested = cache_config.disk_cache || demux_packet_disk_cache_enabled();
        if disk_cache_requested {
            if self.disk_cache.is_none() {
                self.disk_cache = DemuxPacketDiskCache::from_config(&cache_config);
            }
            self.disk_cache_writable = self.disk_cache.is_some();
        } else {
            self.disk_cache_writable = false;
        }

        self.trim_to_limit();
        self.refresh_readahead_hysteresis();
    }

    fn mark_all_seekable_summaries_dirty(&self) {
        for range in self.ranges.values() {
            range.mark_seekable_dirty();
        }
    }
}
