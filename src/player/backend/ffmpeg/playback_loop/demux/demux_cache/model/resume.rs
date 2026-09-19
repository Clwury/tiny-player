use std::cmp::Ordering;

use super::CachedDemuxPacket;

/// A stream's demux-order tail, independent of the seekable presentation-time
/// endpoint and of packet residency. Like mpv's correct_dts/correct_pos, an
/// ordering key is usable only while every appended packet is strictly newer.
#[derive(Clone, Copy, Debug)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct StreamResumePosition {
    dts: Option<i64>,
    pos: Option<i64>,
    correct_dts: bool,
    correct_pos: bool,
}

impl StreamResumePosition {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn new(
        packet: &CachedDemuxPacket,
    ) -> Self {
        Self {
            dts: packet.raw_dts,
            pos: packet.raw_pos,
            correct_dts: packet.raw_dts.is_some(),
            correct_pos: packet.raw_pos.is_some(),
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn observe(
        &mut self,
        packet: &CachedDemuxPacket,
    ) {
        self.correct_dts &= self
            .dts
            .zip(packet.raw_dts)
            .is_some_and(|(old, new)| new > old);
        self.correct_pos &= self
            .pos
            .zip(packet.raw_pos)
            .is_some_and(|(old, new)| new > old);
        self.dts = packet.raw_dts;
        self.pos = packet.raw_pos;
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn resumable(self) -> bool {
        self.correct_dts || self.correct_pos
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn compare(
        self,
        packet: &CachedDemuxPacket,
    ) -> Option<Ordering> {
        let dts = self
            .correct_dts
            .then(|| packet.raw_dts.zip(self.dts))
            .flatten();
        let pos = self
            .correct_pos
            .then(|| packet.raw_pos.zip(self.pos))
            .flatten();
        dts.or(pos).map(|(new, old)| new.cmp(&old))
    }
}
