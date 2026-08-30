use super::*;

#[test]
fn demux_packet_cache_state_uses_local_auto_as_cache_inactive() {
    let config = PlaybackCacheConfig {
        cache_secs: 30.0,
        demuxer_readahead_secs: 2.0,
        cache_pause: true,
        ..PlaybackCacheConfig::default()
    }
    .resolved_for_cacheable_input(false);

    let state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );

    assert_eq!(state.readahead_nsecs, 2_000_000_000);
    assert_eq!(state.backbuffer_limit_bytes, 0);
    assert!(!state.cache_pause_enabled);
}

#[test]
fn demux_packet_cache_state_keeps_forced_seekable_cache_when_local_auto_is_inactive() {
    let config = PlaybackCacheConfig {
        seekable_cache: PlaybackSeekableCacheMode::Enabled,
        demuxer_max_back_bytes: 2048,
        ..PlaybackCacheConfig::default()
    }
    .resolved_for_cacheable_input(false);

    let state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );

    assert_eq!(state.backbuffer_limit_bytes, 2048);
}

#[test]
fn demux_packet_cache_state_allows_zero_cache_secs_to_use_demux_readahead() {
    let config = PlaybackCacheConfig {
        mode: PlaybackCacheMode::Enabled,
        cache_secs: 0.0,
        demuxer_readahead_secs: 2.0,
        ..PlaybackCacheConfig::default()
    };

    let state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );

    assert_eq!(state.readahead_nsecs, 2_000_000_000);
}

#[test]
fn demux_packet_cache_state_caps_readahead_with_configured_packet_limit() {
    let config = PlaybackCacheConfig {
        mode: PlaybackCacheMode::Enabled,
        cache_secs: 120.0,
        demuxer_readahead_secs: 2.0,
        demuxer_packet_max_readahead_secs: 30.0,
        ..PlaybackCacheConfig::default()
    };

    let state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );

    assert_eq!(state.readahead_nsecs, 30_000_000_000);
}

#[test]
fn demux_packet_cache_state_can_disable_packet_readahead_time_cap() {
    let config = PlaybackCacheConfig {
        mode: PlaybackCacheMode::Enabled,
        cache_secs: 120.0,
        demuxer_readahead_secs: 2.0,
        demuxer_packet_max_readahead_secs: 0.0,
        ..PlaybackCacheConfig::default()
    };

    let state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );

    assert_eq!(state.readahead_nsecs, 120_000_000_000);
}

#[test]
fn demux_packet_cache_state_allows_zero_demuxer_max_bytes() {
    let config = PlaybackCacheConfig {
        demuxer_max_bytes: 0,
        ..PlaybackCacheConfig::default()
    };

    let state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        config,
    );

    // A finite shared budget turns the legacy "unlimited" forward setting
    // into the remaining bounded slice. Setting the total budget to zero is
    // the explicit way to retain independent-layer semantics.
    assert!(state.memory_limit_bytes > 0);
    assert!(!state.should_pause_demux());

    let independent = PlaybackCacheConfig {
        demuxer_max_bytes: 0,
        total_cache_max_bytes: 0,
        ..PlaybackCacheConfig::default()
    };
    let independent_state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        independent,
    );
    assert_eq!(independent_state.memory_limit_bytes, 0);
}

#[test]
fn demux_packet_cache_state_applies_live_cache_config() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    state.append_packet(cached_anchor(0, 1_000_000_000));
    state.append_packet(cached_anchor(1_000_000_000, 2_000_000_000));
    state.set_read_index_for_test(1);

    let config = PlaybackCacheConfig {
        mode: PlaybackCacheMode::Disabled,
        cache_secs: 3.0,
        demuxer_readahead_secs: 2.0,
        demuxer_packet_max_readahead_secs: 1.5,
        demuxer_hysteresis_secs: 0.5,
        demuxer_max_bytes: 1024,
        demuxer_max_back_bytes: 2048,
        demuxer_donate_buffer: false,
        cache_pause: false,
        ..PlaybackCacheConfig::default()
    };
    state.cache_buffering_percent = Some(25);
    state.apply_cache_config(config);

    assert_eq!(state.memory_limit_bytes, 1024);
    assert_eq!(state.backbuffer_limit_bytes, 0);
    assert_eq!(state.readahead_nsecs, 1_500_000_000);
    assert_eq!(state.hysteresis_nsecs, 500_000_000);
    assert!(!state.donate_backbuffer);
    assert!(!state.cache_pause_enabled);
    assert_eq!(state.cache_buffering_percent, None);
    assert_eq!(state.read_index, 0);
    assert_eq!(state.cached_bytes, 1024);
}

#[test]
fn demux_packet_cache_state_trims_consumed_packet_at_memory_limit() {
    let mut state = DemuxPacketCacheState::new(
        0,
        0,
        ffi::AVCodecID::AV_CODEC_ID_MPEG4,
        PlaybackSessionId(1),
        cache_config_for_test(),
    );
    let mut packet = cached_packet(0, true, Some(0), Some(1_000_000_000));
    packet.byte_len = DEMUX_PACKET_CACHE_MEMORY_BYTES;
    state.append_packet(packet);

    assert_eq!(state.cached_bytes, DEMUX_PACKET_CACHE_MEMORY_BYTES);
    assert!(state.should_pause_demux());

    state.set_read_index_for_test(1);
    state.reader_nsecs = 1_000_000_000;
    state.trim_to_limit();

    assert_eq!(state.cached_bytes, 0);
    assert_eq!(state.read_index, 0);
    assert_eq!(state.read_range().global_order.len(), 0);
    assert!(!state.should_pause_demux());
}
