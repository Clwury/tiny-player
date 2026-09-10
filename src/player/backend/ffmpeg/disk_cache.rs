use std::{
    collections::BTreeMap,
    fs::File,
    io,
    os::unix::fs::FileExt,
    sync::{Arc, Mutex},
};

/// A file with reusable extents. Readers retain their allocation until the I/O
/// completes, so eviction can never overwrite a packet already handed out.
#[derive(Clone)]
pub(super) struct BoundedDiskFile {
    pub(super) file: Arc<File>,
    state: Arc<Mutex<AllocationState>>,
    maintenance: Arc<Mutex<()>>,
}

struct AllocationState {
    limit: u64,
    free: BTreeMap<u64, u64>,
    live: BTreeMap<u64, u64>,
    tail: u64,
    used: u64,
    file_len: u64,
    shrink_pending: bool,
    maintenance_limit: Option<u64>,
    limit_generation: u64,
}

pub(super) struct DiskBlock {
    pub(super) file: Arc<File>,
    pub(super) offset: u64,
    pub(super) len: usize,
    state: Arc<Mutex<AllocationState>>,
}

impl BoundedDiskFile {
    pub(super) fn new(file: Arc<File>, limit: u64) -> Self {
        Self {
            file,
            state: Arc::new(Mutex::new(AllocationState {
                limit,
                free: BTreeMap::new(),
                live: BTreeMap::new(),
                tail: 0,
                used: 0,
                file_len: 0,
                shrink_pending: false,
                maintenance_limit: None,
                limit_generation: 0,
            })),
            maintenance: Arc::new(Mutex::new(())),
        }
    }

    pub(super) fn reserve(&self, len: usize) -> Option<Arc<DiskBlock>> {
        let len64 = u64::try_from(len).ok().filter(|len| *len > 0)?;
        let mut state = self.state.lock().expect("disk allocation state poisoned");
        let limit = state
            .maintenance_limit
            .unwrap_or(state.limit)
            .min(state.limit);
        let free = state.free.iter().find_map(|(&offset, &size)| {
            (size >= len64 && offset.checked_add(len64).is_some_and(|end| end <= limit))
                .then_some((offset, size))
        });
        let offset = if let Some((offset, size)) = free {
            state.free.remove(&offset);
            if size > len64 {
                state.free.insert(offset + len64, size - len64);
            }
            offset
        } else {
            let offset = state.tail;
            let end = offset.checked_add(len64)?;
            if end > limit {
                return None;
            }
            state.tail = end;
            offset
        };
        state.live.insert(offset, len64);
        state.used += len64;
        Some(Arc::new(DiskBlock {
            file: Arc::clone(&self.file),
            offset,
            len,
            state: Arc::clone(&self.state),
        }))
    }

    #[cfg(test)]
    pub(super) fn allocated_bytes(&self) -> u64 {
        self.state
            .lock()
            .expect("disk allocation state poisoned")
            .used
    }

    pub(super) fn limit(&self) -> u64 {
        self.state
            .lock()
            .expect("disk allocation state poisoned")
            .limit
    }

    pub(super) fn file_len(&self) -> u64 {
        self.state
            .lock()
            .expect("disk allocation state poisoned")
            .file_len
    }

    pub(super) fn set_limit(&self, limit: u64) {
        let mut state = self.state.lock().expect("disk allocation state poisoned");
        state.limit = limit;
        state.shrink_pending = true;
        state.limit_generation = state.limit_generation.wrapping_add(1);
    }

    pub(super) fn maintain_file_size(&self) {
        let Ok(_maintenance) = self.maintenance.try_lock() else {
            return;
        };
        let (limit, generation) = {
            let mut state = self.state.lock().expect("disk allocation state poisoned");
            let live_end = state
                .live
                .last_key_value()
                .map(|(&offset, &len)| offset + len)
                .unwrap_or(0);
            if !state.shrink_pending || live_end > state.limit {
                return;
            }
            // Reservations remain bounded by this target until truncation
            // finishes, even if a concurrent configuration raises the quota.
            state.maintenance_limit = Some(state.limit);
            (state.limit, state.limit_generation)
        };
        // Never perform filesystem I/O while holding the allocation mutex:
        // cache accounting and eviction also acquire it under their own lock.
        let result = self.file.metadata().and_then(|metadata| {
            if metadata.len() > limit {
                self.file.set_len(limit)?;
            }
            Ok(metadata.len().min(limit))
        });
        let mut state = self.state.lock().expect("disk allocation state poisoned");
        state.maintenance_limit = None;
        if let Ok(file_len) = result {
            state.file_len = state.file_len.max(file_len).min(limit);
            if state.limit_generation == generation {
                state.shrink_pending = false;
            }
        }
    }

    pub(super) fn accepts(&self, block: &DiskBlock) -> bool {
        Arc::ptr_eq(&self.state, &block.state)
            && block.offset + block.len as u64
                <= self
                    .state
                    .lock()
                    .expect("disk allocation state poisoned")
                    .limit
    }
}

impl DiskBlock {
    pub(super) fn write(&self, data: &[u8]) -> io::Result<()> {
        if data.len() != self.len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "disk allocation length mismatch",
            ));
        }
        let result = self.file.write_all_at(data, self.offset);
        let actual_len = result
            .as_ref()
            .err()
            .and_then(|_| self.file.metadata().ok())
            .map(|metadata| metadata.len());
        let mut state = self.state.lock().expect("disk allocation state poisoned");
        if result.is_ok() {
            state.file_len = state.file_len.max(self.offset + data.len() as u64);
        } else if let Some(len) = actual_len {
            state.file_len = state.file_len.max(len);
        }
        result
    }

    pub(super) fn read_at(&self, relative: u64, output: &mut [u8]) -> io::Result<usize> {
        let remaining = (self.len as u64).saturating_sub(relative);
        let len = output
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        self.file
            .read_at(&mut output[..len], self.offset + relative)
    }
}

impl Drop for DiskBlock {
    fn drop(&mut self) {
        let mut state = self.state.lock().expect("disk allocation state poisoned");
        state.live.remove(&self.offset);
        state.used -= self.len as u64;
        let mut offset = self.offset;
        let mut len = self.len as u64;
        if let Some((&before, &size)) = state.free.range(..offset).next_back()
            && before + size == offset
        {
            state.free.remove(&before);
            offset = before;
            len += size;
        }
        if let Some(size) = state.free.remove(&(offset + len)) {
            len += size;
        }
        if offset + len == state.tail {
            state.tail = offset;
        } else {
            state.free.insert(offset, len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicted_space_is_reused_only_after_the_last_reader_finishes() {
        let file = Arc::new(tempfile::tempfile().unwrap());
        let pool = BoundedDiskFile::new(Arc::clone(&file), 8);
        let first = pool.reserve(4).unwrap();
        first.write(b"abcd").unwrap();
        let reader = Arc::clone(&first);
        let second = pool.reserve(4).unwrap();
        second.write(b"efgh").unwrap();
        drop(first);
        assert!(pool.reserve(4).is_none());
        let mut restored = [0; 4];
        reader.read_at(0, &mut restored).unwrap();
        assert_eq!(&restored, b"abcd");
        drop(reader);
        for _ in 0..100 {
            let replacement = pool.reserve(4).unwrap();
            replacement.write(b"ijkl").unwrap();
        }
        assert_eq!(file.metadata().unwrap().len(), 8);
        assert_eq!(pool.allocated_bytes(), 4);
    }

    #[test]
    fn shrinking_waits_for_in_flight_allocations_and_then_truncates() {
        let file = Arc::new(tempfile::tempfile().unwrap());
        let pool = BoundedDiskFile::new(Arc::clone(&file), 8);
        let first = pool.reserve(4).unwrap();
        let pending = pool.reserve(4).unwrap();
        first.write(b"abcd").unwrap();
        pool.set_limit(4);
        pending.write(b"efgh").unwrap();
        assert!(!pool.accepts(&pending));
        drop(pending);
        pool.maintain_file_size();
        assert_eq!(file.metadata().unwrap().len(), 4);
        assert!(pool.reserve(1).is_none());
    }
}
