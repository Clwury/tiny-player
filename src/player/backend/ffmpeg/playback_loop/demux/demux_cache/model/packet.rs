use std::{
    fs::File,
    os::raw::c_int,
    sync::{Arc, Mutex},
    time::Instant,
};

use super::{
    AvPacket, AvPacketReadDiagnostic, AvPacketStorageKind, DemuxPacketCacheReadTiming,
    DemuxPacketDiskCache, VideoRecoveryPointKind, read_demux_packet_disk_payload,
};

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct CachedDemuxPacket {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) payload:
        CachedDemuxPacketPayload,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) stream_index: c_int,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) timeline_anchor: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) recovery_point: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) recovery_kind:
        VideoRecoveryPointKind,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) safe_seek_point: bool,
    /// Presentation timestamp mapped onto the media timeline without forcing
    /// demux/decode-order packets to be monotonic. This is the timestamp used
    /// for mpv-compatible cached-seek boundaries and anchor selection.
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) seek_timestamp_nsecs:
        Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) raw_pts: Option<i64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) raw_dts: Option<i64>,
    /// Monotonic packet window retained for forward-buffer accounting and
    /// playback scheduling. It must not define OSC seekable ranges.
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) start_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) end_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) byte_len: usize,
}

#[derive(Clone, Copy)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct CachedDemuxPacketRecovery
{
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) recovery_point: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) recovery_kind:
        VideoRecoveryPointKind,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) safe_seek_point: bool,
}

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) enum CachedDemuxPacketPayload {
    Memory(Arc<Mutex<AvPacket>>),
    Disk {
        props: Arc<Mutex<AvPacket>>,
        offset: u64,
        len: usize,
    },
}

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxPacketReadSource {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) stream_offset: usize,
    payload: DemuxPacketReadPayload,
    diagnostic: Option<AvPacketReadDiagnostic>,
}

enum DemuxPacketReadPayload {
    Memory(Arc<Mutex<AvPacket>>),
    Disk {
        file: Arc<File>,
        props: Arc<Mutex<AvPacket>>,
        offset: u64,
        len: usize,
    },
}

impl DemuxPacketReadSource {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn packet_ref(
        self,
        timing: &mut DemuxPacketCacheReadTiming,
    ) -> std::result::Result<(AvPacket, usize), String> {
        let started_at = Instant::now();
        let mut packet = match self.payload {
            DemuxPacketReadPayload::Memory(packet) => {
                let packet = packet
                    .lock()
                    .map_err(|_| "FFmpeg demux packet cache packet lock poisoned".to_string())?;
                AvPacket::ref_from(&packet)?
            }
            DemuxPacketReadPayload::Disk {
                file,
                props,
                offset,
                len,
            } => {
                let disk_read_started_at = Instant::now();
                let data = read_demux_packet_disk_payload(&file, offset, len)?;
                timing.disk_read += disk_read_started_at.elapsed();
                timing.disk_reads = timing.disk_reads.saturating_add(1);
                let props = props
                    .lock()
                    .map_err(|_| "FFmpeg demux packet cache packet lock poisoned".to_string())?;
                AvPacket::from_data_and_props(&data, &props)?
            }
        };
        if let Some(diagnostic) = self.diagnostic {
            packet.set_read_diagnostic(diagnostic);
        }
        timing.packet_ref += started_at.elapsed();
        Ok((packet, self.stream_offset))
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn set_diagnostic(
        &mut self,
        diagnostic: AvPacketReadDiagnostic,
    ) {
        self.diagnostic = Some(diagnostic);
    }
}

impl CachedDemuxPacket {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn seek_end_nsecs(
        &self,
    ) -> Option<u64> {
        let seek_start_nsecs = self.seek_timestamp_nsecs?;
        let mapped_start_nsecs = self.start_nsecs?;
        let duration_nsecs = self
            .end_nsecs
            .and_then(|end_nsecs| end_nsecs.checked_sub(mapped_start_nsecs))
            .unwrap_or_default();
        Some(seek_start_nsecs.saturating_add(duration_nsecs))
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn seek_block_timestamp_nsecs(
        &self,
    ) -> Option<u64> {
        let seek_timestamp_nsecs = self.seek_timestamp_nsecs?;
        if self.raw_pts.is_some() || self.raw_dts.is_some() {
            // Runtime packets match mpv's compute_keyframe_times(): only the
            // packet PTS/DTS contributes to a closed recovery block's max.
            Some(seek_timestamp_nsecs)
        } else {
            // State-level tests construct synthetic packets without AVPacket
            // timestamps and use their explicit interval to model all frame
            // timestamps represented by that fixture.
            self.seek_end_nsecs().or(Some(seek_timestamp_nsecs))
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn from_packet(
        packet: &AvPacket,
        stream_index: c_int,
        timeline_anchor: bool,
        recovery: CachedDemuxPacketRecovery,
        start_nsecs: Option<u64>,
        end_nsecs: Option<u64>,
        seek_timestamp_nsecs: Option<u64>,
    ) -> std::result::Result<Self, String> {
        Ok(Self {
            payload: CachedDemuxPacketPayload::Memory(Arc::new(Mutex::new(AvPacket::ref_from(
                packet,
            )?))),
            stream_index,
            timeline_anchor,
            recovery_point: recovery.recovery_point,
            recovery_kind: recovery.recovery_kind,
            safe_seek_point: recovery.safe_seek_point,
            seek_timestamp_nsecs,
            raw_pts: packet.pts(),
            raw_dts: packet.dts(),
            start_nsecs,
            end_nsecs,
            byte_len: packet.byte_len(),
        })
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn packet_ref(
        &self,
        disk_cache: Option<&DemuxPacketDiskCache>,
    ) -> std::result::Result<AvPacket, String> {
        let mut timing = DemuxPacketCacheReadTiming::default();
        self.read_source(disk_cache, 0)?
            .packet_ref(&mut timing)
            .map(|(packet, _)| packet)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn read_source(
        &self,
        disk_cache: Option<&DemuxPacketDiskCache>,
        stream_offset: usize,
    ) -> std::result::Result<DemuxPacketReadSource, String> {
        let payload = match &self.payload {
            CachedDemuxPacketPayload::Memory(packet) => {
                DemuxPacketReadPayload::Memory(Arc::clone(packet))
            }
            CachedDemuxPacketPayload::Disk { props, offset, len } => {
                let disk_cache = disk_cache
                    .ok_or_else(|| "FFmpeg demux packet disk cache unavailable".to_string())?;
                DemuxPacketReadPayload::Disk {
                    file: Arc::clone(&disk_cache.file),
                    props: Arc::clone(props),
                    offset: *offset,
                    len: *len,
                }
            }
        };
        Ok(DemuxPacketReadSource {
            stream_offset,
            payload,
            diagnostic: None,
        })
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn storage_kind(
        &self,
    ) -> AvPacketStorageKind {
        match &self.payload {
            CachedDemuxPacketPayload::Memory(_) => AvPacketStorageKind::Memory,
            CachedDemuxPacketPayload::Disk { .. } => AvPacketStorageKind::Disk,
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn spill_to_disk(
        &mut self,
        disk_cache: &mut DemuxPacketDiskCache,
    ) -> std::result::Result<(), String> {
        let packet = match &self.payload {
            CachedDemuxPacketPayload::Memory(packet) => Arc::clone(packet),
            CachedDemuxPacketPayload::Disk { .. } => return Ok(()),
        };
        let (data, props) = {
            let packet = packet
                .lock()
                .map_err(|_| "FFmpeg demux packet cache packet lock poisoned".to_string())?;
            let Some(data) = packet.data() else {
                return Ok(());
            };
            if data.is_empty() {
                return Ok(());
            }
            (data.to_vec(), AvPacket::props_from(&packet)?)
        };
        let offset = disk_cache.write_packet(&data)?;
        self.payload = CachedDemuxPacketPayload::Disk {
            props: Arc::new(Mutex::new(props)),
            offset,
            len: data.len(),
        };
        Ok(())
    }
}
