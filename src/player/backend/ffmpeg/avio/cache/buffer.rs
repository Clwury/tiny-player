use std::{collections::VecDeque, sync::Arc};

use super::HTTP_CACHE_CHUNK_SIZE;

struct BytePage {
    data: Arc<[u8]>,
    start: usize,
    end: usize,
}

impl BytePage {
    fn remaining(&self) -> usize {
        self.end.saturating_sub(self.start)
    }
}

/// An append payload split into independently allocated pages before the HTTP
/// cache state lock is acquired.
pub(in crate::player::backend::ffmpeg::avio::cache) struct PreparedByteAppend {
    pages: VecDeque<BytePage>,
    len: usize,
}

impl PreparedByteAppend {
    pub(in crate::player::backend::ffmpeg::avio::cache) fn from_bytes(data: &[u8]) -> Self {
        Self::from_bytes_with_page_size(data, HTTP_CACHE_CHUNK_SIZE)
    }

    fn from_bytes_with_page_size(data: &[u8], page_size: usize) -> Self {
        let page_size = page_size.max(1);
        let pages = data
            .chunks(page_size)
            .map(|chunk| BytePage {
                data: Arc::<[u8]>::from(chunk),
                start: 0,
                end: chunk.len(),
            })
            .collect();
        Self {
            pages,
            len: data.len(),
        }
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn len(&self) -> usize {
        self.len
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn discard_front(&mut self, len: usize) {
        let mut remaining = len.min(self.len);
        self.len -= remaining;
        while remaining > 0 {
            let Some(front) = self.pages.front_mut() else {
                break;
            };
            let available = front.remaining();
            if remaining < available {
                front.start += remaining;
                remaining = 0;
            } else {
                remaining -= available;
                self.pages.pop_front();
            }
        }
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn from_buffer(
        buffer: ByteRingBuffer,
    ) -> Self {
        Self {
            pages: buffer.pages,
            len: buffer.len,
        }
    }
}

/// A segmented byte FIFO. Unlike a contiguous `Vec<u8>` ring, growing this
/// buffer never reallocates, zero-fills, or copies the bytes already cached.
pub(in crate::player::backend::ffmpeg::avio::cache) struct ByteRingBuffer {
    pages: VecDeque<BytePage>,
    len: usize,
    max_capacity: usize,
    page_size: usize,
}

impl ByteRingBuffer {
    pub(in crate::player::backend::ffmpeg::avio::cache) fn new(max_capacity: usize) -> Self {
        Self::new_with_page_size(max_capacity, HTTP_CACHE_CHUNK_SIZE)
    }

    fn new_with_page_size(max_capacity: usize, page_size: usize) -> Self {
        Self {
            pages: VecDeque::new(),
            len: 0,
            max_capacity: max_capacity.max(1),
            page_size: page_size.max(1),
        }
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn new_with_page_size_for_test(
        max_capacity: usize,
        page_size: usize,
    ) -> Self {
        Self::new_with_page_size(max_capacity, page_size)
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn len(&self) -> usize {
        self.len
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn max_capacity(&self) -> usize {
        self.max_capacity
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn clear(&mut self) {
        self.pages.clear();
        self.len = 0;
    }

    /// Convenience path for state-only tests and small internal moves. Network
    /// append paths must call `PreparedByteAppend::from_bytes` before locking
    /// and then use `append_prepared`.
    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg::avio::cache) fn append(&mut self, data: &[u8]) {
        let prepared = PreparedByteAppend::from_bytes_with_page_size(data, self.page_size);
        self.append_prepared(prepared);
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn append_prepared(
        &mut self,
        mut prepared: PreparedByteAppend,
    ) {
        if prepared.is_empty() {
            return;
        }
        let required_len = self
            .len
            .checked_add(prepared.len())
            .expect("HTTP stream cache buffer length overflowed");
        assert!(
            required_len <= self.max_capacity,
            "HTTP stream cache append exceeds configured capacity"
        );
        while let Some(page) = prepared.pages.pop_front() {
            self.pages.push_back(page);
        }
        self.len = required_len;
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn discard_front(&mut self, len: usize) {
        let mut remaining = len.min(self.len);
        if remaining == 0 {
            return;
        }
        self.len -= remaining;
        while remaining > 0 {
            let Some(front) = self.pages.front_mut() else {
                debug_assert_eq!(self.len, 0);
                break;
            };
            let available = front.remaining();
            if remaining < available {
                front.start += remaining;
                remaining = 0;
            } else {
                remaining -= available;
                self.pages.pop_front();
            }
        }
        if self.len == 0 {
            self.pages.clear();
        }
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn copy_at(
        &self,
        offset: usize,
        output: &mut [u8],
    ) -> usize {
        if output.is_empty() || offset >= self.len {
            return 0;
        }

        let read = (self.len - offset).min(output.len());
        let mut skip = offset;
        let mut written = 0usize;
        for page in &self.pages {
            let available = page.remaining();
            if skip >= available {
                skip -= available;
                continue;
            }
            let source_start = page.start + skip;
            let copy_len = (available - skip).min(read - written);
            output[written..written + copy_len]
                .copy_from_slice(&page.data[source_start..source_start + copy_len]);
            written += copy_len;
            skip = 0;
            if written == read {
                break;
            }
        }
        debug_assert_eq!(written, read);
        written
    }

    /// Detaches the suffix at `offset` without copying cached bytes. A page
    /// crossing the boundary is represented by two Arc-backed slices of the
    /// same slab.
    pub(in crate::player::backend::ffmpeg::avio::cache) fn split_off(
        &mut self,
        offset: usize,
    ) -> Self {
        let offset = offset.min(self.len);
        let original_len = self.len;
        let mut prefix = VecDeque::new();
        let mut suffix = VecDeque::new();
        let mut cursor = 0usize;
        while let Some(mut page) = self.pages.pop_front() {
            let page_len = page.remaining();
            let page_end = cursor.saturating_add(page_len);
            if page_end <= offset {
                prefix.push_back(page);
            } else if cursor >= offset {
                suffix.push_back(page);
            } else {
                let split = page.start.saturating_add(offset.saturating_sub(cursor));
                let suffix_page = BytePage {
                    data: Arc::clone(&page.data),
                    start: split,
                    end: page.end,
                };
                page.end = split;
                if page.remaining() > 0 {
                    prefix.push_back(page);
                }
                if suffix_page.remaining() > 0 {
                    suffix.push_back(suffix_page);
                }
            }
            cursor = page_end;
        }
        self.pages = prefix;
        self.len = offset;
        Self {
            pages: suffix,
            len: original_len.saturating_sub(offset),
            max_capacity: self.max_capacity,
            page_size: self.page_size,
        }
    }

    pub(in crate::player::backend::ffmpeg::avio::cache) fn resize_capacity(
        &mut self,
        max_capacity: usize,
    ) {
        let max_capacity = max_capacity.max(1);
        if self.len > max_capacity {
            self.discard_front(self.len - max_capacity);
        }
        self.max_capacity = max_capacity;
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn page_count_for_test(&self) -> usize {
        self.pages.len()
    }

    #[cfg(test)]
    pub(in crate::player::backend::ffmpeg) fn page_data_ptrs_for_test(&self) -> Vec<usize> {
        self.pages
            .iter()
            .map(|page| page.data.as_ptr() as usize)
            .collect()
    }
}
