//! Stream implementations for hybrid VFS.
//!
//! This module provides InputStream and OutputStream implementations for
//! reading and writing files through the hybrid VFS.

use std::sync::Arc;

use bytes::Bytes;
use system_interface::fs::FileIoExt;
use tokio::sync::RwLock;
use wasmtime_wasi_io::poll::Pollable;
use wasmtime_wasi_io::streams::{InputStream, OutputStream, StreamError, StreamResult};

use crate::storage::VfsStorage;

/// An input stream for reading from a real filesystem file.
pub struct RealFileInputStream {
    /// The underlying file.
    file: Arc<cap_std::fs::File>,
    /// Current read position.
    position: u64,
    /// Whether the stream has been closed.
    closed: bool,
}

impl RealFileInputStream {
    /// Create a new file input stream starting at the given offset.
    pub fn new(file: Arc<cap_std::fs::File>, offset: u64) -> Self {
        Self {
            file,
            position: offset,
            closed: false,
        }
    }
}

#[async_trait::async_trait]
impl Pollable for RealFileInputStream {
    async fn ready(&mut self) {
        // File I/O is always "ready" - we do blocking reads
        // In a production system, this would use async file I/O
    }
}

#[async_trait::async_trait]
impl InputStream for RealFileInputStream {
    fn read(&mut self, size: usize) -> StreamResult<Bytes> {
        if self.closed {
            return Err(StreamError::Closed);
        }

        if size == 0 {
            return Ok(Bytes::new());
        }

        let mut buf = vec![0u8; size];
        match self.file.read_at(&mut buf, self.position) {
            Ok(0) => {
                // EOF
                self.closed = true;
                Err(StreamError::Closed)
            }
            Ok(n) => {
                buf.truncate(n);
                self.position += n as u64;
                Ok(Bytes::from(buf))
            }
            Err(e) => Err(StreamError::LastOperationFailed(e.into())),
        }
    }

    async fn blocking_read(&mut self, size: usize) -> StreamResult<Bytes> {
        // For file I/O, blocking_read is the same as read since file reads
        // don't actually block in the async sense (they complete synchronously)
        self.read(size)
    }
}

/// An output stream for writing to a real filesystem file.
pub struct RealFileOutputStream {
    /// The underlying file.
    file: Arc<cap_std::fs::File>,
    /// Current write position.
    position: u64,
    /// Whether to append to the file.
    append: bool,
    /// Whether the stream has been closed.
    closed: bool,
}

impl RealFileOutputStream {
    /// Create a new file output stream for writing at a specific offset.
    pub fn write_at(file: Arc<cap_std::fs::File>, offset: u64) -> Self {
        Self {
            file,
            position: offset,
            append: false,
            closed: false,
        }
    }

    /// Create a new file output stream for appending.
    pub fn append(file: Arc<cap_std::fs::File>) -> Self {
        Self {
            file,
            position: 0, // Position doesn't matter for append
            append: true,
            closed: false,
        }
    }
}

#[async_trait::async_trait]
impl Pollable for RealFileOutputStream {
    async fn ready(&mut self) {
        // File I/O is always "ready"
    }
}

#[async_trait::async_trait]
impl OutputStream for RealFileOutputStream {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        if self.closed {
            return Err(StreamError::Closed);
        }

        if bytes.is_empty() {
            return Ok(());
        }

        let result = if self.append {
            // For append mode, get the file length and write there
            // since cap-std doesn't have a direct append_at method
            match self.file.metadata() {
                Ok(meta) => {
                    let len = meta.len();
                    self.file.write_at(&bytes, len)
                }
                Err(e) => Err(e),
            }
        } else {
            self.file.write_at(&bytes, self.position)
        };

        match result {
            Ok(n) => {
                if !self.append {
                    self.position += n as u64;
                }
                Ok(())
            }
            Err(e) => Err(StreamError::LastOperationFailed(e.into())),
        }
    }

    fn flush(&mut self) -> StreamResult<()> {
        if self.closed {
            return Err(StreamError::Closed);
        }
        // cap-std File doesn't have a flush method that we can call synchronously
        // in the way OutputStream expects. The sync will happen on close.
        Ok(())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        if self.closed {
            return Err(StreamError::Closed);
        }
        // We can always write - return a reasonable buffer size
        Ok(64 * 1024) // 64KB
    }

    async fn blocking_write_and_flush(&mut self, bytes: Bytes) -> StreamResult<()> {
        self.write(bytes)?;
        self.flush()
    }
}

// ============================================================================
// VFS Stream Implementations
// ============================================================================

/// An input stream for reading from a VFS file.
pub struct VfsInputStream<S: VfsStorage + 'static> {
    /// The VFS storage backend.
    storage: Arc<S>,
    /// Path to the file.
    path: String,
    /// Current read position.
    position: u64,
    /// Whether the stream has been closed.
    closed: bool,
}

impl<S: VfsStorage + 'static> VfsInputStream<S> {
    /// Create a new VFS input stream starting at the given offset.
    pub fn new(storage: Arc<S>, path: String, offset: u64) -> Self {
        Self {
            storage,
            path,
            position: offset,
            closed: false,
        }
    }

    /// Read from VFS storage synchronously.
    fn read_sync(&mut self, size: usize) -> StreamResult<Bytes> {
        if size == 0 {
            return Ok(Bytes::new());
        }

        let storage = Arc::clone(&self.storage);
        let path = self.path.clone();
        let position = self.position;

        // Use a thread to run the async operation to avoid blocking the current runtime
        let result = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("Failed to create runtime: {e}"))?;

            rt.block_on(async { storage.read_at(&path, position, size as u64).await })
                .map_err(|e| format!("VFS read failed: {:?}", e))
        })
        .join()
        .map_err(|_| StreamError::trap("Thread panicked during VFS read"))?;

        match result {
            Ok(data) => {
                if data.is_empty() {
                    self.closed = true;
                    Err(StreamError::Closed)
                } else {
                    self.position += data.len() as u64;
                    Ok(Bytes::from(data))
                }
            }
            Err(e) => Err(StreamError::LastOperationFailed(anyhow::anyhow!("{}", e))),
        }
    }
}

#[async_trait::async_trait]
impl<S: VfsStorage + 'static> Pollable for VfsInputStream<S> {
    async fn ready(&mut self) {
        // VFS reads are always "ready"
    }
}

#[async_trait::async_trait]
impl<S: VfsStorage + 'static> InputStream for VfsInputStream<S> {
    fn read(&mut self, size: usize) -> StreamResult<Bytes> {
        if self.closed {
            return Err(StreamError::Closed);
        }
        self.read_sync(size)
    }

    async fn blocking_read(&mut self, size: usize) -> StreamResult<Bytes> {
        self.read(size)
    }
}

/// An output stream for writing to a VFS file.
///
/// This stream buffers writes and flushes them to VFS storage.
/// Since VFS operations are async but stream writes are sync, we use
/// a buffer and flush strategy.
pub struct VfsOutputStream<S: VfsStorage + 'static> {
    /// The VFS storage backend.
    storage: Arc<S>,
    /// Path to the file.
    path: String,
    /// Write buffer.
    buffer: Arc<RwLock<Vec<u8>>>,
    /// Current write position in the file.
    position: u64,
    /// Whether the stream has been closed.
    closed: bool,
}

impl<S: VfsStorage + 'static> VfsOutputStream<S> {
    /// Create a new VFS output stream for writing at a specific offset.
    pub fn write_at(storage: Arc<S>, path: String, offset: u64) -> Self {
        Self {
            storage,
            path,
            buffer: Arc::new(RwLock::new(Vec::new())),
            position: offset,
            closed: false,
        }
    }

    /// Flush the buffer to VFS storage synchronously.
    ///
    /// This is tricky because VFS storage is async but this method is sync.
    /// We spawn a thread with its own tokio runtime to avoid blocking issues.
    fn flush_sync(&mut self) -> StreamResult<()> {
        // Try to get the buffer without blocking
        let buffer_data = if let Ok(mut buf) = self.buffer.try_write() {
            std::mem::take(&mut *buf)
        } else {
            // If we can't get the lock, try with blocking
            let mut buf = self.buffer.blocking_write();
            std::mem::take(&mut *buf)
        };

        if buffer_data.is_empty() {
            return Ok(());
        }

        // Write to VFS storage
        // We spawn a blocking thread to handle the async operation
        let storage = Arc::clone(&self.storage);
        let path = self.path.clone();
        let position = self.position;
        let data_len = buffer_data.len() as u64;

        // Use a thread to run the async operation to avoid blocking the current runtime
        let result = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("Failed to create runtime: {e}"))?;

            rt.block_on(async { storage.write_at(&path, position, &buffer_data).await })
                .map_err(|e| format!("VFS write failed: {:?}", e))
        })
        .join()
        .map_err(|_| StreamError::trap("Thread panicked during VFS write"))?;

        match result {
            Ok(()) => {
                self.position += data_len;
                Ok(())
            }
            Err(e) => Err(StreamError::LastOperationFailed(anyhow::anyhow!("{}", e))),
        }
    }
}

#[async_trait::async_trait]
impl<S: VfsStorage + 'static> Pollable for VfsOutputStream<S> {
    async fn ready(&mut self) {
        // VFS writes are always "ready" since we buffer
    }
}

#[async_trait::async_trait]
impl<S: VfsStorage + 'static> OutputStream for VfsOutputStream<S> {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        if self.closed {
            return Err(StreamError::Closed);
        }

        if bytes.is_empty() {
            return Ok(());
        }

        // Add to buffer
        if let Ok(mut buf) = self.buffer.try_write() {
            buf.extend_from_slice(&bytes);
        } else {
            let mut buf = self.buffer.blocking_write();
            buf.extend_from_slice(&bytes);
        }

        // Flush if buffer is large enough
        let buf_len = if let Ok(buf) = self.buffer.try_read() {
            buf.len()
        } else {
            let buf = self.buffer.blocking_read();
            buf.len()
        };

        if buf_len >= 64 * 1024 {
            self.flush_sync()?;
        }

        Ok(())
    }

    fn flush(&mut self) -> StreamResult<()> {
        if self.closed {
            return Err(StreamError::Closed);
        }
        self.flush_sync()
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        if self.closed {
            return Err(StreamError::Closed);
        }
        // We can always write - return a reasonable buffer size
        Ok(64 * 1024) // 64KB
    }

    async fn blocking_write_and_flush(&mut self, bytes: Bytes) -> StreamResult<()> {
        self.write(bytes)?;
        self.flush()
    }
}

impl<S: VfsStorage + 'static> Drop for VfsOutputStream<S> {
    fn drop(&mut self) {
        // Try to flush any remaining buffered data
        let _ = self.flush_sync();
    }
}
