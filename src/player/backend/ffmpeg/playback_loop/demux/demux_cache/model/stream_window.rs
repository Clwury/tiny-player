use std::os::raw::c_int;

use crate::player::backend::StreamCacheKind;

use super::packet::CachedDemuxPacket;

#[derive(Clone, Copy)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct StreamForwardWindow {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) stream_index: c_int,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) kind: StreamCacheKind,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) reader_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) end_nsecs: u64,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) has_forward_packet: bool,
}

impl StreamForwardWindow {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn duration_nsecs(
        self,
    ) -> u64 {
        self.end_nsecs.saturating_sub(self.reader_nsecs)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct StreamForwardState {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) reader_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) end_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) packet_count: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) bytes: usize,
}

impl StreamForwardState {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn push_packet(
        &mut self,
        packet: &CachedDemuxPacket,
    ) {
        self.push_packet_parts(packet.byte_len, packet.start_nsecs, packet.end_nsecs);
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn push_packet_parts(
        &mut self,
        byte_len: usize,
        start_nsecs: Option<u64>,
        end_nsecs: Option<u64>,
    ) {
        self.packet_count = self.packet_count.saturating_add(1);
        self.bytes = self.bytes.saturating_add(byte_len);
        if let Some(start_nsecs) = start_nsecs {
            self.reader_nsecs = Some(self.reader_nsecs.unwrap_or(start_nsecs).min(start_nsecs));
        }
        if let Some(end_nsecs) = end_nsecs.or(start_nsecs) {
            self.end_nsecs = Some(self.end_nsecs.unwrap_or(end_nsecs).max(end_nsecs));
        }
    }
}

#[derive(Default)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct StreamCacheRangeState {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) reader_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) cache_end_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) has_forward_packet: bool,
}
