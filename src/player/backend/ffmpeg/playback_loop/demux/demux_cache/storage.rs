#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use super::AvPacket;
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use super::{
    CacheUnlinkPolicy, PlaybackCacheConfig,
};

#[path = "storage/disk.rs"]
mod disk;

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) use disk::{
    DemuxPacketDiskCache, demux_packet_disk_cache_enabled, read_demux_packet_disk_payload,
};
