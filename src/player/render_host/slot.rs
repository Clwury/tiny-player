use std::{
    mem,
    ops::Deref,
    sync::{Arc, Mutex},
};

#[derive(Clone, Default, Debug)]
pub struct FrameBufferPool {
    inner: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl FrameBufferPool {
    const MAX_RETAINED_BUFFERS: usize = 12;

    pub fn rent(&self, min_capacity: usize) -> PooledBytes {
        let mut buffers = self.inner.lock().expect("frame buffer pool poisoned");
        let index = buffers
            .iter()
            .position(|buffer| buffer.capacity() >= min_capacity)
            .unwrap_or_else(|| buffers.len());
        let mut bytes = if index < buffers.len() {
            buffers.swap_remove(index)
        } else {
            Vec::with_capacity(min_capacity)
        };
        bytes.clear();
        PooledBytes {
            bytes,
            pool: Some(self.clone()),
        }
    }

    fn recycle(&self, mut bytes: Vec<u8>) {
        bytes.clear();
        let mut buffers = self.inner.lock().expect("frame buffer pool poisoned");
        if buffers.len() < Self::MAX_RETAINED_BUFFERS {
            buffers.push(bytes);
        }
    }
}

#[derive(Debug)]
pub struct PooledBytes {
    bytes: Vec<u8>,
    pool: Option<FrameBufferPool>,
}

impl PooledBytes {
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        Self { bytes, pool: None }
    }

    pub fn extend_from_slice(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.bytes.as_mut_ptr()
    }

    pub fn resize(&mut self, len: usize, value: u8) {
        self.bytes.resize(len, value);
    }

    pub fn into_vec(mut self) -> Vec<u8> {
        self.pool = None;
        mem::take(&mut self.bytes)
    }
}

impl Clone for PooledBytes {
    fn clone(&self) -> Self {
        Self::from_vec(self.bytes.clone())
    }
}

impl Deref for PooledBytes {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

impl Drop for PooledBytes {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.recycle(mem::take(&mut self.bytes));
        }
    }
}

impl From<Vec<u8>> for PooledBytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self::from_vec(bytes)
    }
}

impl PartialEq for PooledBytes {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for PooledBytes {}
