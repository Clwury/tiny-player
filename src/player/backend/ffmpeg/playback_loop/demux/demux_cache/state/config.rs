use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    os::raw::c_int,
    time::Instant,
};

use ffmpeg_sys_next as ffi;

use super::{
    DemuxCachedRange, DemuxPacketCacheState, DemuxPacketDiskCache, DemuxSelectedStreams,
    PlaybackCacheConfig, PlaybackCacheMode, PlaybackSessionId, PreparedSeekableRangeReport,
    StreamCacheKind, audio_codec_requires_recovery_point, demux_packet_cache_hysteresis_nsecs,
    demux_packet_cache_readahead_nsecs, demux_packet_disk_cache_enabled, seconds_to_nsecs,
    video_cached_seek_preroll_nsecs,
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
        let disk_cache = DemuxPacketDiskCache::from_config(&cache_config).map(std::sync::Arc::new);
        let disk_cache_writable = disk_cache.is_some();
        let disk_budget_bytes = disk_cache
            .as_ref()
            .map(|cache| usize::try_from(cache.limit()).unwrap_or(usize::MAX))
            .unwrap_or(0);
        let memory_limit_bytes =
            usize::try_from(cache_config.effective_demuxer_max_bytes()).unwrap_or(usize::MAX);
        let cache_active = !matches!(cache_config.mode, PlaybackCacheMode::Disabled);
        let seekable_cache_active = cache_config.seekable_cache_active(cache_active);
        let backbuffer_limit_bytes = if seekable_cache_active {
            usize::try_from(cache_config.effective_demuxer_max_back_bytes()).unwrap_or(usize::MAX)
        } else {
            0
        };
        let readahead_nsecs = demux_packet_cache_readahead_nsecs(&cache_config, cache_active);
        let configured_hysteresis_nsecs = seconds_to_nsecs(cache_config.demuxer_hysteresis_secs);
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
            disk_write_blocked: false,
            disk_config_generation: 0,
            pending_disk_config: None,
            disk_budget_bytes,
            resident_bytes: 0,
            disk_cached_bytes: 0,
            disk_read_requests: Default::default(),
            disk_write_requests: Default::default(),
            disk_hot_packets: Default::default(),
            disk_packets: Default::default(),
            disk_restore_requests: Default::default(),
            disk_worker_active: false,
            read_index: 0,
            consumed_packet_ids: HashSet::new(),
            reader_heads: BTreeMap::new(),
            reader_head_positions: BTreeMap::new(),
            reader_head_generations: BTreeMap::new(),
            last_packet_reads: BTreeMap::new(),
            next_packet_read_sequence: 0,
            #[cfg(test)]
            reader_tracking_full_refresh_count: 0,
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
            failed_cached_seek_ranges: HashMap::new(),
            rejected_cached_seek_ranges: HashMap::new(),
            memory_limit_bytes,
            backbuffer_limit_bytes,
            donate_backbuffer: cache_config.demuxer_donate_buffer,
            readahead_nsecs,
            configured_hysteresis_nsecs,
            automatic_hysteresis: cache_config.automatic_hysteresis,
            hysteresis_nsecs,
            max_cached_ranges: cache_config.demuxer_max_ranges,
            hysteresis_active: false,
            cache_pause_enabled: cache_active && cache_config.cache_pause,
            cache_pause_initial: cache_config.cache_pause_initial,
            cache_pause_wait_nsecs,
            cache_buffering_percent: None,
            cached_bytes: 0,
            append_maintenance_packets: 0,
            append_trim_pressure_packets: 0,
            append_trim_active: false,
            append_trim_pending: false,
            read_trim_pressure_packets: 0,
            reader_nsecs,
            exact_seek_target_nsecs: reader_nsecs,
            session_id,
            seek_request: None,
            demux_position_detached: false,
            refreshing_streams: BTreeMap::new(),
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
            last_emitted_demux_cache_state: None,
            prepared_seekable_report: RefCell::new(Some(PreparedSeekableRangeReport {
                generation: 1,
                revision: 0,
                ..PreparedSeekableRangeReport::default()
            })),
            last_seekable_summary_prepare_at: RefCell::new(Some(Instant::now())),
            seekability_revision: 0,
            last_emitted_seekability_revision: None,
            cache_state_emit_dirty: false,
            demux_input_generation: 0,
            generation: 0,
            producer_recovery_error: None,
            producer_recovery_consecutive_errors: 0,
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
            self.rebuild_range_stream_seek_boundaries(range_id, stream_index);
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn stream_requires_recovery_point(
        &self,
        stream_index: c_int,
    ) -> bool {
        self.selected_streams.audio_stream.is_some_and(|stream| {
            stream.index == stream_index && audio_codec_requires_recovery_point(stream.codec_id)
        })
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn recovery_point_stream_index(
        &self,
    ) -> Option<c_int> {
        self.selected_streams
            .audio_stream
            .filter(|stream| audio_codec_requires_recovery_point(stream.codec_id))
            .map(|stream| stream.index)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn set_selected_streams(
        &mut self,
        selected_streams: DemuxSelectedStreams,
    ) {
        // Like mpv's update_stream_selection_state, discard queues for streams
        // that stop being selected. Keeping them would advertise old coverage
        // across the missing packets when the track is enabled again.
        for (previous, next) in [
            (
                self.selected_streams.audio_stream,
                selected_streams.audio_stream,
            ),
            (
                self.selected_streams.subtitle_stream,
                selected_streams.subtitle_stream,
            ),
        ] {
            if let Some(previous) = previous
                && next.is_none_or(|next| next.index != previous.index)
            {
                self.discard_stream_packets(previous.index);
            }
        }
        for (previous, next) in [
            (
                self.selected_streams.audio_stream,
                selected_streams.audio_stream,
            ),
            (
                self.selected_streams.subtitle_stream,
                selected_streams.subtitle_stream,
            ),
        ] {
            if let Some(next) = next
                && previous.is_none_or(|previous| previous.index != next.index)
            {
                for range in self.ranges.values_mut() {
                    if !range.global_order.is_empty()
                        && !range.stream_queues.contains_key(&next.index)
                    {
                        // This track was not read with the retained video. Its
                        // missing prefix/suffix is not a natural BOF/EOF gap.
                        let boundary = range.ensure_stream_boundary(next.index);
                        boundary.is_bof = false;
                        boundary.is_eof = false;
                    }
                }
            }
        }
        self.selected_streams = selected_streams;
        self.stream_kinds
            .retain(|_, kind| !matches!(kind, StreamCacheKind::Audio | StreamCacheKind::Subtitle));
        if let Some(audio_stream) = selected_streams.audio_stream {
            self.set_stream_kind(audio_stream.index, StreamCacheKind::Audio);
        }
        if let Some(subtitle_stream) = selected_streams.subtitle_stream {
            self.set_stream_kind(subtitle_stream.index, StreamCacheKind::Subtitle);
        }
        self.refreshing_streams
            .retain(|stream_index, _| self.stream_kinds.contains_key(stream_index));
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
            usize::try_from(cache_config.effective_demuxer_max_bytes()).unwrap_or(usize::MAX);
        self.backbuffer_limit_bytes = if seekable_cache_active {
            usize::try_from(cache_config.effective_demuxer_max_back_bytes()).unwrap_or(usize::MAX)
        } else {
            0
        };
        self.donate_backbuffer = cache_config.demuxer_donate_buffer;
        self.max_cached_ranges = cache_config.demuxer_max_ranges;
        self.append_trim_pressure_packets = 0;
        self.append_trim_active = false;
        self.append_trim_pending = false;
        self.read_trim_pressure_packets = 0;
        self.readahead_nsecs = demux_packet_cache_readahead_nsecs(&cache_config, cache_active);
        self.configured_hysteresis_nsecs = seconds_to_nsecs(cache_config.demuxer_hysteresis_secs);
        self.automatic_hysteresis = cache_config.automatic_hysteresis;
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
        self.disk_budget_bytes = if disk_cache_requested && self.disk_cache.is_some() {
            usize::try_from(cache_config.effective_disk_cache_budgets().1).unwrap_or(usize::MAX)
        } else {
            0
        };
        self.disk_cache_writable = disk_cache_requested && self.disk_cache.is_some();
        self.disk_config_generation = self.disk_config_generation.wrapping_add(1);
        self.pending_disk_config = Some(cache_config);
        // The disk worker creates/resizes files and restores out-of-budget
        // payloads without holding this mutex or blocking the coordinator.
        self.trim_to_limit();
        self.enforce_cached_range_limit();
        self.refresh_readahead_hysteresis();
    }

    fn mark_all_seekable_summaries_dirty(&mut self) {
        for range in self.ranges.values() {
            range.mark_seekable_dirty();
        }
        self.bump_seekability_revision();
    }
}
