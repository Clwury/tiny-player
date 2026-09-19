use std::{
    os::raw::c_int,
    sync::{Arc, Mutex},
    time::Instant,
};

use crate::player::backend::ffmpeg::disk_cache::DiskBlock;

use super::{
    AvPacket, AvPacketReadDiagnostic, AvPacketStorageKind, DemuxPacketCacheReadTiming,
    DemuxPacketDiskCache, VideoRecoveryPointKind, read_demux_packet_disk_payload,
};

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct CachedDemuxPacket {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) payload:
        CachedDemuxPacketPayload,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) stream_index: c_int,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) timeline_anchor: bool,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) demux_keyframe: bool,
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
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) raw_pos: Option<i64>,
    /// Monotonic packet window retained for forward-buffer accounting and
    /// playback scheduling. It must not define OSC seekable ranges.
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) start_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) end_nsecs: Option<u64>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) byte_len: usize,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) properties_byte_len: usize,
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
        block: Arc<DiskBlock>,
        hot: Option<Arc<Mutex<AvPacket>>>,
    },
}

#[cfg(test)]
pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct PreparedDemuxPacketDiskSpill
{
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) data: Vec<u8>,
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) props: AvPacket,
}

pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) struct DemuxPacketReadSource {
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) stream_offset: usize,
    payload: DemuxPacketReadPayload,
    diagnostic: Option<AvPacketReadDiagnostic>,
}

enum DemuxPacketReadPayload {
    Memory(Arc<Mutex<AvPacket>>),
    Disk {
        props: Arc<Mutex<AvPacket>>,
        block: Arc<DiskBlock>,
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
            DemuxPacketReadPayload::Disk { block, props } => {
                let disk_read_started_at = Instant::now();
                let data = read_demux_packet_disk_payload(&block.file, block.offset, block.len)?;
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
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn is_cached_seek_anchor(
        &self,
    ) -> bool {
        // Like mpv's demux keyframe runs, cached video seeks also accept
        // AV_PKT_FLAG_KEY on open-GOP/non-IDR packets. Keep decoder recovery
        // metadata strict: those packets cannot reset a broken reference chain.
        // Audio still requires its codec-specific recovery point (e.g. TrueHD).
        self.recovery_point || (self.timeline_anchor && self.demux_keyframe)
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn cached_seek_anchor_kind(
        &self,
    ) -> VideoRecoveryPointKind {
        if self.recovery_kind.is_recovery_point() {
            self.recovery_kind
        } else if self.timeline_anchor && self.demux_keyframe {
            VideoRecoveryPointKind::Keyframe
        } else {
            VideoRecoveryPointKind::None
        }
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn prepare_disk_spill(
        &self,
    ) -> std::result::Result<Option<PreparedDemuxPacketDiskSpill>, String> {
        let packet = match &self.payload {
            CachedDemuxPacketPayload::Memory(packet) => Arc::clone(packet),
            CachedDemuxPacketPayload::Disk { .. } => return Ok(None),
        };
        let packet = packet
            .lock()
            .map_err(|_| "FFmpeg demux packet cache packet lock poisoned".to_string())?;
        let Some(data) = packet.data() else {
            return Ok(None);
        };
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(PreparedDemuxPacketDiskSpill {
            data: data.to_vec(),
            props: AvPacket::props_from(&packet)?,
        }))
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn finish_disk_spill(
        &mut self,
        block: Arc<DiskBlock>,
        spill: PreparedDemuxPacketDiskSpill,
        keep_hot: bool,
    ) {
        if !matches!(&self.payload, CachedDemuxPacketPayload::Memory(_)) {
            return;
        }
        let hot = keep_hot.then(|| self.memory_packet()).flatten();
        self.payload = CachedDemuxPacketPayload::Disk {
            hot,
            props: Arc::new(Mutex::new(spill.props)),
            block,
        };
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn memory_packet(
        &self,
    ) -> Option<Arc<Mutex<AvPacket>>> {
        match &self.payload {
            CachedDemuxPacketPayload::Memory(packet) => Some(Arc::clone(packet)),
            CachedDemuxPacketPayload::Disk { hot, .. } => hot.clone(),
        }
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn resident_bytes(
        &self,
    ) -> usize {
        // Include a conservative per-entry estimate for queue/index nodes and
        // AVPacket properties. Decoder-held references are accounted separately
        // from cache ownership; this is not a process RSS measurement.
        let (resident, copies) = match &self.payload {
            CachedDemuxPacketPayload::Memory(_) => (true, 1usize),
            CachedDemuxPacketPayload::Disk { hot, .. } => {
                (hot.is_some(), 1 + usize::from(hot.is_some()))
            }
        };
        std::mem::size_of::<Self>()
            .saturating_add(512)
            .saturating_add(self.properties_byte_len.saturating_mul(copies))
            .saturating_add(if resident {
                self.byte_len.saturating_add(64)
            } else {
                0
            })
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn disk_bytes(
        &self,
    ) -> usize {
        match &self.payload {
            CachedDemuxPacketPayload::Memory(_) => 0,
            CachedDemuxPacketPayload::Disk { block, .. } => block.len,
        }
    }

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
            demux_keyframe: packet.is_key(),
            recovery_point: recovery.recovery_point,
            recovery_kind: recovery.recovery_kind,
            safe_seek_point: recovery.safe_seek_point,
            seek_timestamp_nsecs,
            raw_pts: packet.pts(),
            raw_dts: packet.dts(),
            raw_pos: packet.position(),
            start_nsecs,
            end_nsecs,
            byte_len: packet.byte_len(),
            properties_byte_len: packet.properties_byte_len(),
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
        _disk_cache: Option<&DemuxPacketDiskCache>,
        stream_offset: usize,
    ) -> std::result::Result<DemuxPacketReadSource, String> {
        let payload = match &self.payload {
            CachedDemuxPacketPayload::Memory(packet) => {
                DemuxPacketReadPayload::Memory(Arc::clone(packet))
            }
            CachedDemuxPacketPayload::Disk {
                hot: Some(packet), ..
            } => DemuxPacketReadPayload::Memory(Arc::clone(packet)),
            CachedDemuxPacketPayload::Disk { props, block, .. } => DemuxPacketReadPayload::Disk {
                block: Arc::clone(block),
                props: Arc::clone(props),
            },
        };
        Ok(DemuxPacketReadSource {
            stream_offset,
            payload,
            diagnostic: None,
        })
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn restore_outside_disk_limit(
        &mut self,
        cache: &DemuxPacketDiskCache,
    ) -> Result<(), String> {
        if let CachedDemuxPacketPayload::Disk { block, .. } = &self.payload
            && !cache.accepts(block)
        {
            let packet = self
                .read_source(Some(cache), 0)?
                .packet_ref(&mut DemuxPacketCacheReadTiming::default())?
                .0;
            self.payload = CachedDemuxPacketPayload::Memory(Arc::new(Mutex::new(packet)));
        }
        Ok(())
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn storage_kind(
        &self,
    ) -> AvPacketStorageKind {
        match &self.payload {
            CachedDemuxPacketPayload::Memory(_) => AvPacketStorageKind::Memory,
            CachedDemuxPacketPayload::Disk { .. } => AvPacketStorageKind::Disk,
        }
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn spill_to_disk(
        &mut self,
        disk_cache: &DemuxPacketDiskCache,
    ) -> std::result::Result<(), String> {
        let Some(spill) = self.prepare_disk_spill()? else {
            return Ok(());
        };
        if let Some(block) = disk_cache.reserve_packet(spill.data.len()) {
            block
                .write(&spill.data)
                .map_err(|error| error.to_string())?;
            self.finish_disk_spill(block, spill, false);
        }
        Ok(())
    }
}
