use super::HTTP_CACHE_CHUNK_SIZE;

pub(in crate::player::backend::ffmpeg::avio::cache) struct ByteRingBuffer {
    pub(in crate::player::backend::ffmpeg::avio::cache) storage: Vec<u8>,
    pub(in crate::player::backend::ffmpeg::avio::cache) head: usize,
    pub(in crate::player::backend::ffmpeg::avio::cache) len: usize,
    pub(in crate::player::backend::ffmpeg::avio::cache) max_capacity: usize,
}

impl ByteRingBuffer {
    pub(in crate::player::backend::ffmpeg::avio::cache) fn new(max_capacity: usize) -> Self {
        Self {
            storage: Vec::new(),
            head: 0,
            len: 0,
            max_capacity,
        }
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn len(&self) -> usize {
        self.len
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn max_capacity(&self) -> usize {
        self.max_capacity
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn append(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        let required_len = self
            .len
            .checked_add(data.len())
            .expect("HTTP stream cache buffer length overflowed");
        debug_assert!(required_len <= self.max_capacity);
        self.ensure_storage_len(required_len);

        let write_offset = (self.head + self.len) % self.storage.len();
        copy_into_wrapped(&mut self.storage, write_offset, data);
        self.len = required_len;
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn discard_front(&mut self, len: usize) {
        let len = len.min(self.len);
        if len == 0 {
            return;
        }
        if len == self.len {
            self.clear();
            return;
        }

        self.head = (self.head + len) % self.storage.len();
        self.len -= len;
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn copy_at(
        &self,
        offset: usize,
        output: &mut [u8],
    ) -> usize {
        if output.is_empty() || offset >= self.len || self.storage.is_empty() {
            return 0;
        }

        let read = (self.len - offset).min(output.len());
        let read_offset = (self.head + offset) % self.storage.len();
        copy_from_wrapped(&self.storage, read_offset, &mut output[..read]);
        read
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn ensure_storage_len(
        &mut self,
        required_len: usize,
    ) {
        if required_len <= self.storage.len() {
            return;
        }

        let new_len = self.grown_storage_len(required_len);
        if self.head == 0 {
            self.storage.resize(new_len, 0);
            return;
        }

        let mut storage = vec![0; new_len];
        self.copy_at(0, &mut storage[..self.len]);
        self.storage = storage;
        self.head = 0;
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn grown_storage_len(
        &self,
        required_len: usize,
    ) -> usize {
        let mut len = if self.storage.is_empty() {
            HTTP_CACHE_CHUNK_SIZE
                .min(self.max_capacity)
                .max(required_len)
        } else {
            self.storage.len()
        };
        while len < required_len {
            let next = len.saturating_mul(2).min(self.max_capacity);
            if next == len {
                break;
            }
            len = next;
        }
        len.max(required_len).min(self.max_capacity)
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn resize_capacity(
        &mut self,
        max_capacity: usize,
    ) {
        let max_capacity = max_capacity.max(1);
        if self.len > max_capacity {
            self.discard_front(self.len - max_capacity);
        }
        if self.storage.len() > max_capacity {
            let mut storage = vec![0; self.len];
            self.copy_at(0, &mut storage);
            self.storage = storage;
            self.head = 0;
        }
        self.max_capacity = max_capacity;
    }
}

pub(in crate::player::backend::ffmpeg::avio::cache) fn copy_into_wrapped(
    storage: &mut [u8],
    offset: usize,
    data: &[u8],
) {
    let front_len = data.len().min(storage.len() - offset);
    storage[offset..offset + front_len].copy_from_slice(&data[..front_len]);
    if front_len < data.len() {
        storage[..data.len() - front_len].copy_from_slice(&data[front_len..]);
    }
}

pub(in crate::player::backend::ffmpeg::avio::cache) fn copy_from_wrapped(
    storage: &[u8],
    offset: usize,
    output: &mut [u8],
) {
    let output_len = output.len();
    let front_len = output_len.min(storage.len() - offset);
    output[..front_len].copy_from_slice(&storage[offset..offset + front_len]);
    if front_len < output_len {
        output[front_len..].copy_from_slice(&storage[..output_len - front_len]);
    }
}
