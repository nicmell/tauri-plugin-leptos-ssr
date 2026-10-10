use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use axum::body::{Body, Bytes, HttpBody};
use axum::http::Response;
use axum::http::response::Parts;
use http_body_util::BodyExt;
use http_body_util::combinators::Fuse;
use tokio::sync::Notify;

use crate::registry::Registry;
use crate::{Error, Result};

/// The data a read collects from frames that are already waiting; a frame is
/// never split, so one read can exceed it by one frame.
pub(crate) const CHUNK: usize = 64 * 1024;

// A read that WebKit cancels comes back over postMessage (src/calls.rs) and
// would race the first one for the next data. WebKit cancels on navigation,
// when the plugin drops the page's streams anyway. Without one, a pending
// request stayed open 20 min (macOS 26.6.2).
/// The longest a command waits before it answers.
pub(crate) const IDLE: Duration = Duration::from_secs(20);

/// What one read returns.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Chunk {
    /// Data, more to come.
    Data(Vec<u8>),
    /// The last data, possibly empty.
    Last(Vec<u8>),
    /// Nothing within the idle limit; read again.
    Idle,
}

impl Chunk {
    /// The chunk as `fetch.js` decodes it: a flag byte (0 data, 1 last,
    /// 2 idle) and the data.
    pub(crate) fn into_bytes(self) -> Vec<u8> {
        let (flag, data) = match self {
            Self::Data(data) => (0, data),
            Self::Last(data) => (1, data),
            Self::Idle => (2, Vec::new()),
        };
        let mut bytes = Vec::with_capacity(1 + data.len());
        bytes.push(flag);
        bytes.extend_from_slice(&data);
        bytes
    }
}

struct Entry {
    body: tokio::sync::Mutex<Fuse<Body>>,
    cancelled: AtomicBool,
    notify: Notify,
}

impl Entry {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

/// The response bodies that pages read through `fetch_read_body`, per webview.
#[derive(Default)]
pub(crate) struct Streams {
    registry: Registry<Entry>,
}

impl Streams {
    /// The page generation of `webview`, to hand to [`Streams::start`].
    pub(crate) fn generation(&self, webview: &str) -> u64 {
        self.registry.generation(webview)
    }

    /// Splits `response` into its head, the bytes already available, and the
    /// id of the stream holding the rest, if any.
    pub(crate) async fn start(
        &self,
        webview: &str,
        generation: u64,
        response: Response<Body>,
    ) -> Result<(Parts, Vec<u8>, Option<u64>)> {
        let (parts, body) = response.into_parts();
        if body
            .size_hint()
            .exact()
            .is_some_and(|size| size <= CHUNK as u64)
        {
            let collected = body
                .collect()
                .await
                .map_err(|error| Error::Stream(error.to_string()))?;
            return Ok((parts, collected.to_bytes().to_vec(), None));
        }
        let mut body = body.fuse();
        let mut initial = Vec::new();
        if take_ready(&mut body, &mut initial)? {
            return Ok((parts, initial, None));
        }
        let id = self.open(webview, generation, body)?;
        Ok((parts, initial, Some(id)))
    }

    fn open(&self, webview: &str, generation: u64, body: Fuse<Body>) -> Result<u64> {
        let entry = Arc::new(Entry {
            body: tokio::sync::Mutex::new(body),
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
        });
        self.registry
            .insert(webview, generation, entry)
            .ok_or(Error::StreamClosed)
    }

    /// The next chunk of stream `id`, waiting at most `idle` for data.
    pub(crate) async fn read(&self, webview: &str, id: u64, idle: Duration) -> Result<Chunk> {
        let entry = self
            .registry
            .get(webview, id)
            .ok_or(Error::UnknownStream(id))?;
        let cancelled = entry.notify.notified();
        tokio::pin!(cancelled);
        if entry.cancelled.load(Ordering::Acquire) {
            return Err(Error::StreamCancelled);
        }

        let mut body = entry.body.lock().await;
        let first = tokio::select! {
            () = &mut cancelled => return Err(Error::StreamCancelled),
            () = tokio::time::sleep(idle) => return Ok(Chunk::Idle),
            first = next_data(&mut body) => first,
        };
        let chunk = match first {
            Ok(None) => Ok(Chunk::Last(Vec::new())),
            Ok(Some(data)) => {
                let mut data = data.to_vec();
                take_ready(&mut body, &mut data).map(|ended| {
                    if ended {
                        Chunk::Last(data)
                    } else {
                        Chunk::Data(data)
                    }
                })
            }
            Err(error) => Err(error),
        };
        drop(body);
        if !matches!(chunk, Ok(Chunk::Data(_))) {
            drop(self.registry.remove(webview, id));
        }
        chunk
    }

    /// Drops stream `id`; a pending read returns at once.
    pub(crate) fn cancel(&self, webview: &str, id: u64) {
        if let Some(entry) = self.registry.remove(webview, id) {
            entry.cancel();
        }
    }

    /// Drops every stream of `webview` and refuses the ones its current page
    /// still has in flight.
    pub(crate) fn close_webview(&self, webview: &str) {
        for entry in self.registry.close_webview(webview) {
            entry.cancel();
        }
    }
}

/// The next non-empty data frame; trailers are skipped.
async fn next_data(body: &mut Fuse<Body>) -> Result<Option<Bytes>> {
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|error| Error::Stream(error.to_string()))?;
        if let Ok(data) = frame.into_data()
            && !data.is_empty()
        {
            return Ok(Some(data));
        }
    }
    Ok(None)
}

/// Appends the frames that are ready now, whole, until `CHUNK`; true when the
/// body ended.
fn take_ready(body: &mut Fuse<Body>, data: &mut Vec<u8>) -> Result<bool> {
    let mut cx = Context::from_waker(Waker::noop());
    while data.len() < CHUNK && !body.is_end_stream() {
        match Pin::new(&mut *body).poll_frame(&mut cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Ok(frame_data) = frame.into_data() {
                    data.extend_from_slice(&frame_data);
                }
            }
            Poll::Ready(Some(Err(error))) => return Err(Error::Stream(error.to_string())),
            Poll::Ready(None) => return Ok(true),
            Poll::Pending => return Ok(false),
        }
    }
    Ok(body.is_end_stream())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use tokio::sync::mpsc;

    use super::*;
    use crate::testing;

    const SHORT: Duration = Duration::from_millis(100);

    fn channel() -> (
        mpsc::UnboundedSender<Bytes>,
        Response<Body>,
        Arc<AtomicUsize>,
    ) {
        let (tx, body, drops) = testing::channel();
        (tx, Response::new(body), drops)
    }

    fn send(tx: &mpsc::UnboundedSender<Bytes>, data: &'static [u8]) {
        tx.send(Bytes::from_static(data)).expect("receiver alive");
    }

    async fn started(streams: &Streams, response: Response<Body>) -> (Vec<u8>, Option<u64>) {
        let (_, initial, id) = streams
            .start("main", streams.generation("main"), response)
            .await
            .expect("started");
        (initial, id)
    }

    #[tokio::test]
    async fn complete_bodies_need_no_stream() {
        let streams = Streams::default();
        let (initial, id) = started(&streams, Response::new(Body::from("whole"))).await;
        assert_eq!(initial, b"whole");
        assert_eq!(id, None);
    }

    #[tokio::test]
    async fn chunks_arrive_before_the_body_ends() {
        let streams = Streams::default();
        let (tx, response, _) = channel();
        send(&tx, b"first");
        let (initial, id) = started(&streams, response).await;
        assert_eq!(initial, b"first");
        let id = id.expect("still streaming");

        send(&tx, b"second");
        let chunk = streams.read("main", id, SHORT).await.expect("read");
        assert_eq!(chunk, Chunk::Data(b"second".to_vec()));

        assert_eq!(
            streams.read("main", id, SHORT).await.expect("read"),
            Chunk::Idle
        );

        send(&tx, b"third");
        drop(tx);
        let chunk = streams.read("main", id, SHORT).await.expect("read");
        assert_eq!(chunk, Chunk::Last(b"third".to_vec()));
        assert!(matches!(
            streams.read("main", id, SHORT).await,
            Err(Error::UnknownStream(_))
        ));
    }

    #[tokio::test]
    async fn frames_are_whole_and_empty_ones_skipped() {
        let streams = Streams::default();
        let (tx, response, _) = channel();
        let (_, id) = started(&streams, response).await;
        let id = id.expect("streaming");

        let big = vec![7u8; CHUNK + 10];
        tx.send(Bytes::new()).expect("receiver alive");
        send(&tx, b"abc");
        tx.send(Bytes::from(big.clone())).expect("receiver alive");
        send(&tx, b"next");
        let Chunk::Data(data) = streams.read("main", id, SHORT).await.expect("read") else {
            panic!("expected data");
        };
        assert_eq!(&data[..3], b"abc");
        assert_eq!(&data[3..], &big[..]);

        drop(tx);
        assert_eq!(
            streams.read("main", id, SHORT).await.expect("read"),
            Chunk::Last(b"next".to_vec())
        );
    }

    #[tokio::test]
    async fn an_ended_body_is_not_polled_again() {
        let streams = Streams::default();
        let (tx, response, _) = channel();
        let (_, id) = started(&streams, response).await;
        let id = id.expect("streaming");
        send(&tx, b"only");
        drop(tx);
        assert_eq!(
            streams.read("main", id, SHORT).await.expect("read"),
            Chunk::Last(b"only".to_vec())
        );
    }

    #[tokio::test]
    async fn cancel_wakes_a_pending_read() {
        let streams = Arc::new(Streams::default());
        let (_tx, response, drops) = channel();
        let (_, id) = started(&streams, response).await;
        let id = id.expect("streaming");

        let reader = {
            let streams = streams.clone();
            tokio::spawn(async move { streams.read("main", id, Duration::from_secs(10)).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        streams.cancel("main", id);
        let read = tokio::time::timeout(Duration::from_secs(1), reader)
            .await
            .expect("woken")
            .expect("joined");
        assert!(matches!(read, Err(Error::StreamCancelled)));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn closing_a_webview_wakes_its_reads_only() {
        let streams = Arc::new(Streams::default());
        let (_tx_main, main, _) = channel();
        let (_tx_other, other, _) = channel();
        let (_, main_id) = started(&streams, main).await;
        let (_, _, other_id) = streams.start("other", 0, other).await.expect("started");

        let reader = {
            let streams = streams.clone();
            let id = main_id.expect("streaming");
            tokio::spawn(async move { streams.read("main", id, Duration::from_secs(10)).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        streams.close_webview("main");
        let read = tokio::time::timeout(Duration::from_secs(1), reader)
            .await
            .expect("woken")
            .expect("joined");
        assert!(matches!(read, Err(Error::StreamCancelled)));
        assert_eq!(
            streams
                .read("other", other_id.expect("streaming"), SHORT)
                .await
                .expect("still open"),
            Chunk::Idle
        );
    }

    #[tokio::test]
    async fn a_late_response_of_an_old_page_is_dropped() {
        let streams = Streams::default();
        let generation = streams.generation("main");
        streams.close_webview("main");
        let (_tx, response, drops) = channel();
        let result = streams.start("main", generation, response).await;
        assert!(matches!(result, Err(Error::StreamClosed)));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn streams_belong_to_their_webview() {
        let streams = Streams::default();
        let (_tx, response, _) = channel();
        let (_, id) = started(&streams, response).await;
        assert!(matches!(
            streams.read("other", id.expect("streaming"), SHORT).await,
            Err(Error::UnknownStream(_))
        ));
    }

    #[test]
    fn chunks_carry_their_flag() {
        assert_eq!(Chunk::Data(b"a".to_vec()).into_bytes(), b"\x00a");
        assert_eq!(Chunk::Last(Vec::new()).into_bytes(), b"\x01");
        assert_eq!(Chunk::Idle.into_bytes(), b"\x02");
    }
}
