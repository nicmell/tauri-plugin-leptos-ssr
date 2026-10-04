//! A response body the test feeds frame by frame.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use axum::body::{Body, Bytes, HttpBody};
use http_body::Frame;
use tokio::sync::mpsc;

/// Panics when polled after its end, like `futures::stream::unfold`, and
/// counts its drops.
struct ChannelBody {
    rx: mpsc::UnboundedReceiver<Bytes>,
    ended: bool,
    drops: Arc<AtomicUsize>,
}

impl HttpBody for ChannelBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        assert!(!self.ended, "polled after its end");
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(data)) => Poll::Ready(Some(Ok(Frame::data(data)))),
            Poll::Ready(None) => {
                self.ended = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for ChannelBody {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// A body, the sender of its frames, and its drop count; dropping the sender
/// ends the body.
pub(crate) fn channel() -> (mpsc::UnboundedSender<Bytes>, Body, Arc<AtomicUsize>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let drops = Arc::new(AtomicUsize::new(0));
    let body = ChannelBody {
        rx,
        ended: false,
        drops: drops.clone(),
    };
    (tx, Body::new(body), drops)
}
