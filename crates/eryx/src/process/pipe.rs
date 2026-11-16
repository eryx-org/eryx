//! In-memory pipes for child processes' stdio (#890).
//!
//! A bounded buffer shared by a writer end and a reader end, each of which
//! may have several handles (wasmtime-wasi asks a stdio stream for a fresh
//! stream per guest handle). Both sides are used directly: the child's
//! stdio streams read and write the buffer synchronously, and the parent's
//! `stream<u8>` producer and consumer poll it, so a write is one append and
//! a wake, with no background task in between.
//!
//! The buffer holds at most its capacity: a full pipe makes the writer wait
//! for space, as an OS pipe does. Once every reader handle is gone, writes
//! fail (`EPIPE`-like); once every writer handle is gone and the buffer is
//! drained, reads see end of file.

use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use wasmtime_wasi::cli::{IsTerminal, StdinStream, StdoutStream};
use wasmtime_wasi::p2::{InputStream, OutputStream, Pollable, StreamError, StreamResult};

/// How long a child may write into a pipe before it yields to the reader.
///
/// A write never blocks while there's space, and the reader is polled from
/// the same task, so without a yield a child writing short lines runs on
/// until the pipe fills (or an epoch tick) before its reader sees any of it.
/// Yielding on every write instead costs a trip through the parent's event
/// loop per line (~100 µs, the cost #890 is about). A timeslice of about
/// one such trip lets a reader take many lines per trip while adding little
/// latency: longer ones (1 ms) slowed a pipeline's teardown after `head`
/// exits, since each stage writes on for a slice before it sees `EPIPE`.
const YIELD_EVERY: Duration = Duration::from_micros(100);

/// How long a WASI 0.3 child's write waits for its reader after a slice.
const READER_GRACE: Duration = Duration::from_millis(1);

/// A pipe holding at most `capacity` bytes.
pub(super) fn pipe(capacity: usize) -> (Writer, Reader) {
    let shared = Arc::new(Shared {
        capacity,
        state: Mutex::new(State {
            buf: BytesMut::new(),
            writers: 1,
            readers: 1,
            readable: Vec::new(),
            writable: Vec::new(),
        }),
    });
    (Writer(Arc::clone(&shared), None, None), Reader(shared))
}

struct Shared {
    capacity: usize,
    state: Mutex<State>,
}

struct State {
    buf: BytesMut,
    /// Live handles of each end.
    writers: usize,
    readers: usize,
    /// Tasks waiting for data (or end of file), and for space (or no reader).
    readable: Vec<Waker>,
    writable: Vec<Waker>,
}

fn wake(wakers: &mut Vec<Waker>) {
    for waker in wakers.drain(..) {
        waker.wake();
    }
}

/// Wakers kept per direction before they're all woken to make room.
const MAX_WAKERS: usize = 8;

fn register(wakers: &mut Vec<Waker>, cx: &Context<'_>) {
    if wakers.iter().any(|w| w.will_wake(cx.waker())) {
        return;
    }
    // Several handles may wait on one end, so keep a list. A waiter that
    // cancels leaves a stale waker behind; rather than let them pile up,
    // wake the lot (live waiters poll again and re-register).
    if wakers.len() >= MAX_WAKERS {
        wake(wakers);
    }
    wakers.push(cx.waker().clone());
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock leaves the buffer consistent.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Append as much of `data` as fits, or wait for space. `None` once
    /// there's no reader.
    fn poll_write(&self, cx: &Context<'_>, data: &[u8]) -> Poll<Option<usize>> {
        let mut state = self.state();
        if state.readers == 0 {
            return Poll::Ready(None);
        }
        let n = data.len().min(self.capacity - state.buf.len());
        if n == 0 && !data.is_empty() {
            register(&mut state.writable, cx);
            return Poll::Pending;
        }
        state.buf.extend_from_slice(&data[..n]);
        wake(&mut state.readable);
        Poll::Ready(Some(n))
    }

    /// Move up to `out.len()` bytes out, or wait for some. `Some(0)` is end
    /// of file.
    fn poll_read(&self, cx: &Context<'_>, out: &mut [u8]) -> Poll<usize> {
        let mut state = self.state();
        if state.buf.is_empty() {
            if state.writers == 0 || out.is_empty() {
                return Poll::Ready(0);
            }
            register(&mut state.readable, cx);
            return Poll::Pending;
        }
        let n = out.len().min(state.buf.len());
        out[..n].copy_from_slice(&state.buf[..n]);
        state.buf.advance(n);
        wake(&mut state.writable);
        Poll::Ready(n)
    }
}

/// A pipe's writing end: a child's stdout or stderr, or a child's stdin from
/// the parent's side, with when its child's writes last finished yielding,
/// and a WASI 0.3 child's wait for its reader.
pub(super) struct Writer(
    Arc<Shared>,
    Option<Instant>,
    Option<Pin<Box<tokio::time::Sleep>>>,
);

/// A pipe's reading end.
pub(super) struct Reader(Arc<Shared>);

impl Clone for Writer {
    fn clone(&self) -> Self {
        self.0.state().writers += 1;
        Self(Arc::clone(&self.0), None, None)
    }
}

impl Clone for Reader {
    fn clone(&self) -> Self {
        self.0.state().readers += 1;
        Self(Arc::clone(&self.0))
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.writers -= 1;
        if state.writers == 0 {
            wake(&mut state.readable);
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let mut state = self.0.state();
        state.readers -= 1;
        if state.readers == 0 {
            wake(&mut state.writable);
        }
    }
}

impl Writer {
    /// Whether this handle has written for `YIELD_EVERY` since its last
    /// yield ended.
    fn slice_used(&self) -> bool {
        self.1.is_none_or(|t| t.elapsed() >= YIELD_EVERY)
    }

    pub(super) fn poll_write(&self, cx: &Context<'_>, data: &[u8]) -> Poll<Option<usize>> {
        self.0.poll_write(cx, data)
    }
}

impl Reader {
    pub(super) fn poll_read(&self, cx: &Context<'_>, out: &mut [u8]) -> Poll<usize> {
        self.0.poll_read(cx, out)
    }
}

// The child's side: wasmtime-wasi stdio streams, p2 and (for p3) async.

impl IsTerminal for Writer {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl IsTerminal for Reader {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for Writer {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(self.clone())
    }

    fn p2_stream(&self) -> Box<dyn OutputStream> {
        Box::new(self.clone())
    }
}

impl StdinStream for Reader {
    fn async_stream(&self) -> Box<dyn AsyncRead + Send + Sync> {
        Box::new(self.clone())
    }

    fn p2_stream(&self) -> Box<dyn InputStream> {
        Box::new(self.clone())
    }
}

#[async_trait::async_trait]
impl Pollable for Writer {
    /// Ready once there's space, or no reader (the next write fails).
    async fn ready(&mut self) {
        // A WASI 0.2 child's writes; see `YIELD_EVERY`.
        if self.slice_used() {
            tokio::task::yield_now().await;
            self.1 = Some(Instant::now());
        }
        std::future::poll_fn(|cx| {
            let mut state = self.0.state();
            if state.readers == 0 || state.buf.len() < self.0.capacity {
                Poll::Ready(())
            } else {
                register(&mut state.writable, cx);
                Poll::Pending
            }
        })
        .await;
    }
}

impl OutputStream for Writer {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        let mut state = self.0.state();
        if state.readers == 0 {
            return Err(StreamError::Closed);
        }
        // The guest may ignore `check_write`; trap as wasmtime-wasi's own
        // streams do, rather than buffer past the bound in host memory.
        if bytes.len() > self.0.capacity - state.buf.len() {
            return Err(StreamError::trap("write exceeded budget"));
        }
        state.buf.extend_from_slice(&bytes);
        wake(&mut state.readable);
        Ok(())
    }

    fn flush(&mut self) -> StreamResult<()> {
        // Written bytes are in the pipe already.
        Ok(())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        let state = self.0.state();
        if state.readers == 0 {
            return Err(StreamError::Closed);
        }
        Ok(self.0.capacity.saturating_sub(state.buf.len()))
    }
}

#[async_trait::async_trait]
impl Pollable for Reader {
    /// Ready once there's data, or end of file.
    async fn ready(&mut self) {
        std::future::poll_fn(|cx| {
            let mut state = self.0.state();
            if state.writers == 0 || !state.buf.is_empty() {
                Poll::Ready(())
            } else {
                register(&mut state.readable, cx);
                Poll::Pending
            }
        })
        .await;
    }
}

impl InputStream for Reader {
    fn read(&mut self, size: usize) -> StreamResult<Bytes> {
        let mut state = self.0.state();
        if state.buf.is_empty() {
            return if state.writers == 0 {
                Err(StreamError::Closed)
            } else {
                Ok(Bytes::new())
            };
        }
        let n = size.min(state.buf.len());
        let data = state.buf.split_to(n).freeze();
        wake(&mut state.writable);
        Ok(data)
    }
}

impl AsyncWrite for Writer {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        // A WASI 0.3 child's writes; see `YIELD_EVERY`. The child runs its
        // own event loop, which polls a self-woken write again at once
        // rather than yield to the parent. So wait for the reader to take
        // what's been written, or for `READER_GRACE` if it doesn't (it may
        // be waiting for this writer to finish).
        let this = &mut *self;
        if this.slice_used() {
            let mut state = this.0.state();
            if state.readers > 0 && !state.buf.is_empty() {
                let grace = this
                    .2
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep(READER_GRACE)));
                if grace.as_mut().poll(cx).is_pending() {
                    register(&mut state.writable, cx);
                    return Poll::Pending;
                }
            }
            drop(state);
            this.2 = None;
            this.1 = Some(Instant::now());
        }
        self.0
            .poll_write(cx, data)
            .map(|n| n.ok_or_else(|| std::io::ErrorKind::BrokenPipe.into()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // The end closes when its last handle is dropped.
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for Reader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let out = buf.initialize_unfilled();
        self.0.poll_read(cx, out).map(|n| {
            buf.advance(n);
            Ok(())
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Data flows, a full pipe makes the writer wait, and each end sees the
    /// other close.
    #[tokio::test]
    async fn backpressure_and_close() {
        let (mut w, mut r) = pipe(4);
        assert_eq!(OutputStream::check_write(&mut w).unwrap(), 4);
        OutputStream::write(&mut w, Bytes::from_static(b"abcd")).unwrap();
        assert_eq!(OutputStream::check_write(&mut w).unwrap(), 0);
        // Full: not ready until the reader takes some.
        let ready = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            Pollable::ready(&mut w),
        );
        assert!(ready.await.is_err());
        assert_eq!(&InputStream::read(&mut r, 2).unwrap()[..], b"ab");
        Pollable::ready(&mut w).await;
        assert_eq!(OutputStream::check_write(&mut w).unwrap(), 2);

        // Async ends, and end of file once the writer is gone.
        let mut w2 = w.clone();
        drop(w);
        w2.write_all(b"ef").await.unwrap();
        drop(w2);
        let mut rest = Vec::new();
        r.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"cdef");
        assert!(matches!(
            InputStream::read(&mut r, 1),
            Err(StreamError::Closed)
        ));

        // Writes past `check_write` trap, even split across handles.
        let (mut w, _r) = pipe(4);
        let mut w2 = w.clone();
        assert!(matches!(
            OutputStream::write(&mut w, Bytes::from_static(b"abcde")),
            Err(StreamError::Trap(_))
        ));
        OutputStream::write(&mut w, Bytes::from_static(b"abc")).unwrap();
        assert!(matches!(
            OutputStream::write(&mut w2, Bytes::from_static(b"de")),
            Err(StreamError::Trap(_))
        ));
        OutputStream::write(&mut w2, Bytes::from_static(b"d")).unwrap();
        assert_eq!(OutputStream::check_write(&mut w).unwrap(), 0);

        // With no reader, writes fail.
        let (mut w, r) = pipe(4);
        drop(r);
        assert!(matches!(
            OutputStream::check_write(&mut w),
            Err(StreamError::Closed)
        ));
        assert!(w.write_all(b"x").await.is_err());
    }
}
