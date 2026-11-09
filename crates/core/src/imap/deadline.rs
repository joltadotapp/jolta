//! A connection that fails once the server stays silent for too long while
//! Jolta waits on it. A connection can die without closing, such as when a
//! router forgets it or the Mac changes networks; without a limit, the
//! worker waits on it until macOS gives up minutes later, and every queued
//! action and message waits behind it.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Sleep;

#[derive(Debug)]
pub struct Deadline<S> {
    inner: S,
    limit: Duration,
    reading: Option<Pin<Box<Sleep>>>,
    writing: Option<Pin<Box<Sleep>>>,
}

impl<S> Deadline<S> {
    pub fn new(inner: S, limit: Duration) -> Deadline<S> {
        Deadline { inner, limit, reading: None, writing: None }
    }
}

/// Starts the wait on the first `Pending`, ends it on progress, and fails
/// when the wait runs out.
fn watch<T>(timer: &mut Option<Pin<Box<Sleep>>>, limit: Duration, cx: &mut Context<'_>, poll: Poll<io::Result<T>>) -> Poll<io::Result<T>> {
    if poll.is_ready() {
        *timer = None;
        return poll;
    }
    let sleep = timer.get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
    match sleep.as_mut().poll(cx) {
        Poll::Ready(()) => {
            *timer = None;
            Poll::Ready(Err(io::Error::new(io::ErrorKind::TimedOut, format!("the server didn't answer for {} seconds", limit.as_secs()))))
        }
        Poll::Pending => Poll::Pending,
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Deadline<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let poll = Pin::new(&mut this.inner).poll_read(cx, buf);
        watch(&mut this.reading, this.limit, cx, poll)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Deadline<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        let poll = Pin::new(&mut this.inner).poll_write(cx, buf);
        if poll.is_ready() {
            // A new command: the wait for its answer starts now.
            this.reading = None;
        }
        watch(&mut this.writing, this.limit, cx, poll)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let poll = Pin::new(&mut this.inner).poll_flush(cx);
        watch(&mut this.writing, this.limit, cx, poll)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::Instant;

    #[tokio::test]
    async fn a_silent_server_fails_the_read_and_a_slow_one_does_not() {
        let limit = Duration::from_millis(300);
        let (client, mut server) = tokio::io::duplex(64);
        let mut conn = Deadline::new(client, limit);
        let mut buf = [0u8; 8];

        let answer = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            server.write_all(b"* OK").await.unwrap();
        };
        let (read, ()) = tokio::join!(conn.read(&mut buf), answer);
        assert_eq!(read.unwrap(), 4, "an answer within the limit arrives");

        tokio::time::sleep(limit * 2).await;
        conn.write_all(b"a NOOP\r\n").await.unwrap();
        let started = Instant::now();
        let err = conn.read(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() >= limit, "the limit counts from the command, not the quiet time before it");
    }
}
