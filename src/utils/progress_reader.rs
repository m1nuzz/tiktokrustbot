use tokio::io::{AsyncRead, ReadBuf};
use std::{pin::Pin, task::{Context, Poll}};

pub struct ProgressReader<R, F>
where
    R: AsyncRead + Unpin,
    F: Fn(u64, u64) + Send + Sync + 'static + Unpin,
{
    inner: R,
    uploaded: u64,
    total: u64,
    on_progress: F,
}

impl<R, F> ProgressReader<R, F>
where
    R: AsyncRead + Unpin,
    F: Fn(u64, u64) + Send + Sync + 'static + Unpin,
{
    pub fn new(inner: R, total: u64, on_progress: F) -> Self {
        Self { inner, uploaded: 0, total, on_progress }
    }
}

impl<R, F> AsyncRead for ProgressReader<R, F>
where
    R: AsyncRead + Unpin,
    F: Fn(u64, u64) + Send + Sync + 'static + Unpin,
{
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let now = buf.filled().len();
                let delta = (now - before) as u64;
                if delta > 0 {
                    this.uploaded += delta;
                    // Call the progress callback by spawning a task to handle async operations
                    (this.on_progress)(this.uploaded, this.total);
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}
