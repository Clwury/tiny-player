use std::{
    sync::{Arc, Weak, atomic::Ordering},
    thread,
    time::Duration,
};

use super::super::{
    AvPacket, CachedDemuxPacketPayload, DemuxPacketCacheShared, DemuxPacketCacheState,
    DemuxPacketDiskCache, PacketId, PlaybackCacheConfig,
};
use crate::player::backend::ffmpeg::disk_cache::DiskBlock;

// One worker owns at most one packet I/O operation. Requests retain packet IDs,
// not payload copies, so a slow/full disk cannot grow an unbounded write queue.
enum StorageWork {
    Configure {
        generation: u64,
        config: PlaybackCacheConfig,
        cache: Option<Arc<DemuxPacketDiskCache>>,
    },
    Write {
        id: PacketId,
        generation: u64,
        packet: Arc<std::sync::Mutex<AvPacket>>,
        cache: Arc<DemuxPacketDiskCache>,
    },
    Read {
        id: PacketId,
        block: Arc<DiskBlock>,
        props: Arc<std::sync::Mutex<AvPacket>>,
        restore: bool,
    },
}

impl DemuxPacketCacheShared {
    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn prepare_storage_work_for_test(
        &self,
    ) -> Option<impl FnOnce() -> bool + '_> {
        let work = self.next_storage_work()?;
        Some(move || self.perform_storage_work(work))
    }

    pub(in crate::player::backend::ffmpeg::playback_loop::demux_cache) fn start_storage_worker(
        self: &Arc<Self>,
    ) -> Result<(), String> {
        if self.disk_worker_started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.state
            .lock()
            .expect("demux cache poisoned")
            .disk_worker_active = true;
        let weak = Arc::downgrade(self);
        if let Err(error) = thread::Builder::new()
            .name("tiny-demux-disk-cache".into())
            .spawn(move || run_storage_worker(weak))
        {
            self.disk_worker_started.store(false, Ordering::Release);
            self.state
                .lock()
                .expect("demux cache poisoned")
                .disk_worker_active = false;
            return Err(format!("创建磁盘缓存工作线程失败：{error}"));
        }
        Ok(())
    }

    fn next_storage_work(&self) -> Option<StorageWork> {
        let mut state = self.state.lock().expect("demux cache poisoned");
        if let Some(config) = state.pending_disk_config.take() {
            return Some(StorageWork::Configure {
                generation: state.disk_config_generation,
                config,
                cache: state.disk_cache.clone(),
            });
        }
        let cache = state.disk_cache.clone()?;
        let wanted = state.desired_hot_packets();
        let stale_requests = state
            .disk_read_requests
            .iter()
            .copied()
            .filter(|id| !state.reader_heads.values().any(|head| head == id))
            .collect::<Vec<_>>();
        for id in stale_requests {
            state.disk_read_requests.remove(&id);
        }
        // Release cold memory copies before either reading or writing. Existing
        // decoder references keep their AVBuffer alive independently.
        let cold = state
            .disk_hot_packets
            .iter()
            .copied()
            .filter(|id| !wanted.contains(id))
            .collect::<Vec<_>>();
        let released_memory = !cold.is_empty();
        for id in cold {
            state.change_packet_storage(id, |packet| {
                if let CachedDemuxPacketPayload::Disk { hot, .. } = &mut packet.payload {
                    *hot = None;
                }
            });
        }
        if released_memory {
            self.refresh_monitor_snapshot(&state);
            self.notify_ready();
        }
        let read_id = state
            .disk_read_requests
            .iter()
            .copied()
            .chain(wanted.iter().copied())
            .find(|id| {
                state.packets.get(id).is_some_and(|packet| {
                    packet.memory_packet().is_none()
                        && (state.disk_read_requests.contains(id)
                            || state.can_load_packet(
                                packet
                                    .byte_len
                                    .saturating_add(packet.properties_byte_len)
                                    .saturating_add(64),
                            ))
                })
            });
        if let Some(id) = read_id
            && let Some(packet) = state.packets.get(&id)
            && let CachedDemuxPacketPayload::Disk { block, props, .. } = &packet.payload
        {
            return Some(StorageWork::Read {
                id,
                block: Arc::clone(block),
                props: Arc::clone(props),
                restore: state.disk_restore_requests.contains(&id),
            });
        }
        if let Some(id) = state.disk_restore_requests.iter().copied().find(|id| {
            state.packets.get(id).is_some_and(|packet| {
                packet.memory_packet().is_some()
                    || state.can_load_packet(
                        packet
                            .byte_len
                            .saturating_add(packet.properties_byte_len)
                            .saturating_add(64),
                    )
            })
        }) {
            let packet = state.packets.get(&id)?;
            if let Some(hot) = packet.memory_packet() {
                state.change_packet_storage(id, |packet| {
                    packet.payload = CachedDemuxPacketPayload::Memory(hot)
                });
                self.notify_ready();
            } else if let CachedDemuxPacketPayload::Disk { block, props, .. } = &packet.payload {
                return Some(StorageWork::Read {
                    id,
                    block: Arc::clone(block),
                    props: Arc::clone(props),
                    restore: true,
                });
            }
        }
        if state.storage_memory_full()
            && (state.disk_write_requests.is_empty()
                || !state.disk_cache_writable
                || state.disk_write_blocked)
            && state.trim_to_limit_for_append_with_outcome().performed
        {
            self.refresh_monitor_snapshot(&state);
            self.notify_ready();
        }
        if state.disk_cache_writable {
            // Spill distant media first. A near-playback packet can remain in
            // memory until it moves out of the hot window or memory fills.
            let id = state.disk_write_requests.iter().rev().copied().find(|id| {
                state.packets.get(id).is_some_and(|packet| {
                    packet.byte_len > 0
                        && packet.byte_len as u64 <= cache.limit()
                        && (!wanted.contains(id) || state.storage_memory_full())
                })
            });
            if let Some(id) = id
                && let Some(packet) = state
                    .packets
                    .get(&id)
                    .and_then(|packet| packet.memory_packet())
            {
                return Some(StorageWork::Write {
                    id,
                    generation: state.disk_config_generation,
                    packet,
                    cache,
                });
            }
        }
        None
    }

    fn perform_storage_work(&self, work: StorageWork) -> bool {
        match work {
            StorageWork::Configure {
                generation,
                config,
                cache,
            } => {
                let requested =
                    config.disk_cache || super::super::demux_packet_disk_cache_enabled();
                let cache = cache.or_else(|| {
                    requested
                        .then(|| DemuxPacketDiskCache::from_config(&config).map(Arc::new))
                        .flatten()
                });
                if let Some(cache) = &cache {
                    cache.set_limit(&config);
                }
                let mut state = self.state.lock().expect("demux cache poisoned");
                if generation != state.disk_config_generation {
                    return true;
                }
                state.disk_cache_writable = requested && cache.is_some();
                state.disk_write_blocked = false;
                state.disk_budget_bytes = if state.disk_cache_writable {
                    cache
                        .as_ref()
                        .map(|cache| usize::try_from(cache.limit()).unwrap_or(usize::MAX))
                        .unwrap_or(0)
                } else {
                    0
                };
                state.disk_cache = cache.clone();
                state.disk_restore_requests = state
                    .disk_packets
                    .iter()
                    .copied()
                    .filter(|id| {
                        state.packets.get(id).is_some_and(|packet| {
                            if let CachedDemuxPacketPayload::Disk { block, .. } = &packet.payload {
                                cache.as_ref().is_some_and(|cache| !cache.accepts(block))
                            } else {
                                false
                            }
                        })
                    })
                    .collect();
                state.mark_cache_state_emit_dirty();
                self.refresh_monitor_snapshot(&state);
                drop(state);
                if let Some(cache) = cache {
                    cache.maintain_file_size();
                }
                self.notify_ready();
                true
            }
            StorageWork::Write {
                id,
                generation,
                packet,
                cache,
            } => {
                // All packet copying and filesystem access happens outside the
                // cache mutex. The original packet remains immediately readable.
                let result = (|| -> Result<Option<(Arc<DiskBlock>, AvPacket)>, String> {
                    let packet = {
                        let guard = packet.lock().map_err(|_| "packet poisoned")?;
                        AvPacket::ref_from(&guard)?
                    };
                    let Some(data) = packet.data().filter(|data| !data.is_empty()) else {
                        return Ok(None);
                    };
                    let Some(block) = cache.reserve_packet(data.len()) else {
                        return Ok(None);
                    };
                    let props = AvPacket::props_from(&packet)?;
                    block.write(data).map_err(|error| error.to_string())?;
                    Ok(Some((block, props)))
                })();
                let mut state = self.state.lock().expect("demux cache poisoned");
                if generation != state.disk_config_generation || !state.disk_cache_writable {
                    return true;
                }
                let same = state
                    .packets
                    .get(&id)
                    .and_then(|cached| cached.memory_packet())
                    .is_some_and(|current| Arc::ptr_eq(&current, &packet));
                if !same {
                    return true;
                }
                match result {
                    Ok(Some((block, props))) if cache.accepts(&block) => {
                        state.disk_write_blocked = false;
                        let keep_hot = state.desired_hot_packets().contains(&id)
                            && !state.storage_memory_full();
                        let hot = keep_hot.then_some(packet);
                        state.change_packet_storage(id, |cached| {
                            cached.payload = CachedDemuxPacketPayload::Disk {
                                props: Arc::new(std::sync::Mutex::new(props)),
                                block,
                                hot,
                            }
                        });
                        self.refresh_monitor_snapshot(&state);
                        drop(state);
                        cache.maintain_file_size();
                        self.notify_ready();
                        true
                    }
                    Err(error) => {
                        tracing::warn!(%error, "disk cache write failed; keeping cached packets in memory");
                        state.disk_cache_writable = false;
                        state.disk_budget_bytes = 0;
                        state.mark_cache_state_emit_dirty();
                        self.notify_ready();
                        false
                    }
                    _ => {
                        state.disk_write_blocked = true;
                        // Reclaim only eligible history, with the normal GOP
                        // and in-flight packet guards and bounded lock time.
                        let freed = state.trim_to_limit_for_append_with_outcome().performed;
                        self.refresh_monitor_snapshot(&state);
                        if freed {
                            self.notify_ready();
                        }
                        freed
                    }
                }
            }
            StorageWork::Read {
                id,
                block,
                props,
                restore,
            } => {
                let result = (|| {
                    let data = super::super::read_demux_packet_disk_payload(
                        &block.file,
                        block.offset,
                        block.len,
                    )?;
                    let props = props
                        .lock()
                        .map_err(|_| "packet properties poisoned".to_string())?;
                    AvPacket::from_data_and_props(&data, &props)
                })();
                let mut state = self.state.lock().expect("demux cache poisoned");
                let same = state.packets.get(&id).is_some_and(|packet| matches!(&packet.payload, CachedDemuxPacketPayload::Disk { block: current, .. } if Arc::ptr_eq(current, &block)));
                if !same {
                    return true;
                }
                match result {
                    Ok(packet) => {
                        let demanded = state.disk_read_requests.contains(&id)
                            && state.reader_heads.values().any(|head| *head == id);
                        // A seek/config change may make speculative I/O stale.
                        // Do not repopulate the old hot window or exceed the new
                        // resident quota with a no-longer-needed read.
                        if !demanded
                            && (!state.can_load_packet(
                                block
                                    .len
                                    .saturating_add(packet.properties_byte_len())
                                    .saturating_add(64),
                            ) || (!restore && !state.desired_hot_packets().contains(&id)))
                        {
                            return false;
                        }
                        let restore = restore
                            && state
                                .disk_cache
                                .as_ref()
                                .is_some_and(|cache| !cache.accepts(&block));
                        let packet = Arc::new(std::sync::Mutex::new(packet));
                        state.change_packet_storage(id, |cached| {
                            if restore {
                                cached.payload = CachedDemuxPacketPayload::Memory(packet);
                            } else if let CachedDemuxPacketPayload::Disk { hot, .. } =
                                &mut cached.payload
                            {
                                *hot = Some(packet);
                            }
                        });
                        self.refresh_monitor_snapshot(&state);
                        let cache = state.disk_cache.clone();
                        drop(state);
                        drop(block);
                        if let Some(cache) = cache {
                            cache.maintain_file_size();
                        }
                        self.notify_ready();
                        true
                    }
                    Err(error) => {
                        // Surface a failed demanded read; never advance its
                        // reader head or hand a corrupted packet to the decoder.
                        state.error = Some(error);
                        self.notify_ready();
                        false
                    }
                }
            }
        }
    }
}

impl DemuxPacketCacheState {
    fn can_load_packet(&self, bytes: usize) -> bool {
        self.resident_limit_bytes() == 0
            || self.resident_bytes.saturating_add(bytes) <= self.resident_limit_bytes()
    }
}

fn run_storage_worker(weak: Weak<DemuxPacketCacheShared>) {
    loop {
        let Some(shared) = weak.upgrade() else {
            return;
        };
        if shared.should_stop() {
            return;
        }
        let observed = shared.control.wake_generation();
        if let Some(work) = shared.next_storage_work()
            && shared.perform_storage_work(work)
        {
            continue;
        }
        let cache = shared
            .state
            .lock()
            .expect("demux cache poisoned")
            .disk_cache
            .clone();
        if let Some(cache) = cache {
            cache.maintain_file_size();
        }
        let control = Arc::clone(&shared.control);
        drop(shared);
        control.wait_for_wake_change(observed, Duration::from_millis(25));
    }
}
