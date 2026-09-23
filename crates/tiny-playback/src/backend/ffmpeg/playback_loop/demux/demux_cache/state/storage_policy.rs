use std::collections::BTreeSet;

use super::{CachedDemuxPacket, DemuxPacketCacheState, PacketId};

impl DemuxPacketCacheState {
    pub(in crate::backend::ffmpeg::playback_loop::demux_cache) fn resident_limit_bytes(
        &self,
    ) -> usize {
        if self.memory_limit_bytes == 0 {
            return 0;
        }
        self.memory_limit_bytes
            .saturating_add(self.backbuffer_limit_bytes)
    }

    pub(in crate::backend::ffmpeg::playback_loop::demux_cache) fn media_limits(
        &self,
    ) -> (usize, usize) {
        let memory = self.resident_limit_bytes();
        if memory == 0 || self.disk_budget_bytes == 0 {
            return (self.memory_limit_bytes, self.backbuffer_limit_bytes);
        }
        let disk_forward = ((self.disk_budget_bytes as u128 * self.memory_limit_bytes as u128)
            / memory as u128) as usize;
        (
            self.memory_limit_bytes.saturating_add(disk_forward),
            self.backbuffer_limit_bytes
                .saturating_add(self.disk_budget_bytes - disk_forward),
        )
    }

    pub(in crate::backend::ffmpeg::playback_loop::demux_cache) fn storage_memory_full(
        &self,
    ) -> bool {
        self.disk_worker_active
            && self.disk_cache.is_some()
            && self.resident_limit_bytes() > 0
            && self.resident_bytes >= self.resident_limit_bytes()
    }

    pub(in crate::backend::ffmpeg::playback_loop::demux_cache) fn track_packet_storage_insert(
        &mut self,
        id: PacketId,
        packet: &CachedDemuxPacket,
    ) {
        self.resident_bytes = self.resident_bytes.saturating_add(packet.resident_bytes());
        self.disk_cached_bytes = self.disk_cached_bytes.saturating_add(packet.disk_bytes());
        if packet.disk_bytes() > 0 {
            self.disk_packets.insert(id);
            if let super::super::CachedDemuxPacketPayload::Disk { block, .. } = &packet.payload
                && self
                    .disk_cache
                    .as_ref()
                    .is_some_and(|cache| !cache.accepts(block))
            {
                self.disk_restore_requests.insert(id);
            }
        } else if packet.byte_len > 0 {
            self.disk_write_requests.insert(id);
        }
    }

    pub(in crate::backend::ffmpeg::playback_loop::demux_cache) fn track_packet_storage_remove(
        &mut self,
        id: PacketId,
        packet: &CachedDemuxPacket,
    ) {
        self.resident_bytes = self.resident_bytes.saturating_sub(packet.resident_bytes());
        self.disk_cached_bytes = self.disk_cached_bytes.saturating_sub(packet.disk_bytes());
        self.disk_write_requests.remove(&id);
        self.disk_read_requests.remove(&id);
        self.disk_restore_requests.remove(&id);
        self.disk_hot_packets.remove(&id);
        self.disk_packets.remove(&id);
    }

    pub(in crate::backend::ffmpeg::playback_loop::demux_cache) fn change_packet_storage(
        &mut self,
        id: PacketId,
        change: impl FnOnce(&mut CachedDemuxPacket),
    ) {
        let Some(mut packet) = self.packets.remove(&id) else {
            return;
        };
        self.track_packet_storage_remove(id, &packet);
        change(&mut packet);
        self.track_packet_storage_insert(id, &packet);
        if packet.disk_bytes() > 0 && packet.memory_packet().is_some() {
            self.disk_hot_packets.insert(id);
        }
        self.packets.insert(id, packet);
        self.mark_cache_state_emit_dirty();
    }

    pub(in crate::backend::ffmpeg::playback_loop::demux_cache) fn desired_hot_packets(
        &self,
    ) -> BTreeSet<PacketId> {
        let mut wanted = self.reader_heads.values().copied().collect::<BTreeSet<_>>();
        wanted.extend(
            self.disk_read_requests
                .iter()
                .copied()
                .filter(|id| self.reader_heads.values().any(|head| head == id)),
        );
        let budget = if self.resident_limit_bytes() == 0 {
            32 * 1024 * 1024
        } else {
            (self.resident_limit_bytes() / 3).min(32 * 1024 * 1024)
        };
        let mut bytes = 0usize;
        for id in self
            .read_range()
            .global_order
            .iter()
            .skip(self.read_index)
            .take(512)
        {
            let Some(packet) = self.packets.get(id) else {
                continue;
            };
            if packet
                .start_nsecs
                .is_some_and(|ts| ts > self.reader_nsecs.saturating_add(3_000_000_000))
            {
                continue;
            }
            bytes = bytes.saturating_add(packet.byte_len);
            if bytes > budget {
                break;
            }
            wanted.insert(*id);
        }
        // Keep a small recent history for repeated short seeks. Forward input
        // always gets first claim on the hot cache's bounded working set.
        for id in self
            .read_range()
            .global_order
            .iter()
            .take(self.read_index)
            .rev()
            .take(128)
        {
            let Some(packet) = self.packets.get(id) else {
                continue;
            };
            if packet
                .end_nsecs
                .is_some_and(|ts| ts.saturating_add(1_000_000_000) < self.reader_nsecs)
            {
                break;
            }
            bytes = bytes.saturating_add(packet.byte_len);
            if bytes > budget {
                break;
            }
            wanted.insert(*id);
        }
        wanted
    }
}
