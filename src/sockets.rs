use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::http::{HeaderValue, Uri};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::Instant;
use tokio_tungstenite::WebSocketStream;
pub(crate) use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{self, ClientRequestBuilder, Message};

use crate::dispatch::{Dispatcher, MARKER, Upgradable};
use crate::registry::Registry;
use crate::streams::CHUNK;
use crate::{Error, Result};

/// The unread bytes a socket keeps for its page before it stops reading.
const READ_AHEAD: usize = 1024 * 1024;

/// How long a closing socket waits for the other side's close.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// The kinds of a record, as `fetch.js` reads and writes them.
const TEXT: u8 = 0;
const BINARY: u8 = 1;
const CLOSE: u8 = 2;
const ERROR: u8 = 3;

/// What a socket hands its page.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Record {
    Text(String),
    Binary(Vec<u8>),
    /// The close the other side sent: the last record, a clean close.
    Close(u16, String),
    /// Why the socket failed: the last record, an unclean close.
    Error(String),
}

impl Record {
    fn payload_len(&self) -> usize {
        match self {
            Self::Text(text) | Self::Error(text) => text.len(),
            Self::Binary(data) => data.len(),
            Self::Close(_, reason) => 2 + reason.len(),
        }
    }

    fn is_last(&self) -> bool {
        matches!(self, Self::Close(..) | Self::Error(_))
    }

    /// Appends the record as `fetch.js` decodes it: a kind byte, the payload
    /// length as a big-endian u32, then the payload. A close carries its
    /// code as a big-endian u16 before the reason.
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.push(match self {
            Self::Text(_) => TEXT,
            Self::Binary(_) => BINARY,
            Self::Close(..) => CLOSE,
            Self::Error(_) => ERROR,
        });
        let length =
            u32::try_from(self.payload_len()).expect("tungstenite caps messages at 64 MiB");
        out.extend_from_slice(&length.to_be_bytes());
        match self {
            Self::Text(text) | Self::Error(text) => out.extend_from_slice(text.as_bytes()),
            Self::Binary(data) => out.extend_from_slice(data),
            Self::Close(code, reason) => {
                out.extend_from_slice(&code.to_be_bytes());
                out.extend_from_slice(reason.as_bytes());
            }
        }
    }

    fn closing(frame: Option<CloseFrame>) -> Self {
        match frame {
            Some(frame) => Self::Close(frame.code.into(), frame.reason.as_str().to_owned()),
            None => Self::Close(CloseCode::Status.into(), String::new()),
        }
    }
}

/// The messages of a `ws_send` body: text and binary records only.
pub(crate) fn decode_sends(mut bytes: &[u8]) -> Result<Vec<Message>> {
    let mut messages = Vec::new();
    while !bytes.is_empty() {
        let invalid = |reason: &str| Error::InvalidRequest(format!("websocket send: {reason}"));
        let (&kind, rest) = bytes.split_first().ok_or_else(|| invalid("no kind"))?;
        let (length, rest) = rest
            .split_first_chunk::<4>()
            .ok_or_else(|| invalid("no length"))?;
        let length = u32::from_be_bytes(*length) as usize;
        if rest.len() < length {
            return Err(invalid("a record is cut short"));
        }
        let (payload, rest) = rest.split_at(length);
        messages.push(match kind {
            TEXT => Message::text(
                std::str::from_utf8(payload).map_err(|_| invalid("text is not UTF-8"))?,
            ),
            BINARY => Message::binary(payload.to_vec()),
            _ => return Err(invalid("unknown kind")),
        });
        bytes = rest;
    }
    Ok(messages)
}

/// What `ws_open` answers.
#[derive(Debug, Serialize)]
pub(crate) struct Opened {
    pub(crate) id: u64,
    pub(crate) protocol: String,
    pub(crate) extensions: String,
}

enum Command {
    /// Messages to write, and who waits for them.
    Send(Vec<Message>, oneshot::Sender<()>),
    Close(Option<CloseFrame>),
}

/// One socket: the records its page has not read, and the queue to its
/// writer.
pub(crate) struct Socket {
    inbox: Mutex<Inbox>,
    /// Wakes the page's read: records arrived, or the page is gone.
    arrived: Notify,
    /// Wakes the reader: the page took records.
    room: Notify,
    /// Wakes the reader: a close went out, so its timeout starts.
    closing: Arc<Notify>,
    /// One read at a time.
    reading: tokio::sync::Mutex<()>,
    commands: mpsc::UnboundedSender<Command>,
}

enum Taken {
    Gone,
    Nothing,
    Batch { batch: Vec<u8>, last: bool },
}

#[derive(Default)]
struct Inbox {
    records: VecDeque<Record>,
    bytes: usize,
    /// The page is gone: reads fail, and data is dropped.
    gone: bool,
}

impl Socket {
    fn inbox(&self) -> MutexGuard<'_, Inbox> {
        self.inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn push(&self, record: Record) {
        let mut inbox = self.inbox();
        if inbox.gone {
            return;
        }
        inbox.bytes += record.payload_len();
        inbox.records.push_back(record);
        drop(inbox);
        self.arrived.notify_one();
    }

    /// The records that arrived, whole, up to [`CHUNK`]; the last record
    /// ends a batch.
    fn take(&self) -> Taken {
        let mut inbox = self.inbox();
        if inbox.gone {
            return Taken::Gone;
        }
        let mut batch = Vec::new();
        while let Some(record) = inbox.records.front() {
            if !batch.is_empty() && batch.len() + 5 + record.payload_len() > CHUNK {
                break;
            }
            let record = inbox.records.pop_front().expect("a front record");
            inbox.bytes -= record.payload_len();
            record.encode(&mut batch);
            if record.is_last() {
                return Taken::Batch { batch, last: true };
            }
        }
        if batch.is_empty() {
            Taken::Nothing
        } else {
            Taken::Batch { batch, last: false }
        }
    }

    fn has_room(&self, read_ahead: usize) -> bool {
        let inbox = self.inbox();
        inbox.gone || inbox.bytes < read_ahead
    }

    fn leave(&self) {
        self.inbox().gone = true;
        self.arrived.notify_one();
        self.room.notify_one();
    }
}

/// The websockets that pages hold open, per webview.
pub(crate) struct Sockets {
    registry: Registry<Socket>,
    read_ahead: usize,
    close_timeout: Duration,
}

impl Default for Sockets {
    fn default() -> Self {
        Self {
            registry: Registry::default(),
            read_ahead: READ_AHEAD,
            close_timeout: CLOSE_TIMEOUT,
        }
    }
}

impl Sockets {
    /// The page generation of `webview`, to hand to [`Sockets::open`].
    pub(crate) fn generation(&self, webview: &str) -> u64 {
        self.registry.generation(webview)
    }

    /// Opens a websocket to `path` through `dispatcher`. The request carries
    /// the plugin's marker and no `Origin`.
    pub(crate) async fn open(
        &self,
        dispatcher: &Dispatcher,
        webview: &str,
        generation: u64,
        path: &str,
        protocols: &[String],
    ) -> Result<Opened> {
        let (stream, host) = dispatcher
            .connect()
            .await
            .map_err(|error| Error::Socket(error.to_string()))?;
        let uri: Uri = format!("ws://{host}{path}").parse().map_err(
            |error: axum::http::uri::InvalidUri| Error::InvalidRequest(error.to_string()),
        )?;
        let mut request =
            ClientRequestBuilder::new(uri).with_header(MARKER.as_str(), crate::ORIGIN);
        for protocol in protocols {
            request = request.with_sub_protocol(protocol.as_str());
        }
        let (socket, response) = match tokio_tungstenite::client_async(request, stream).await {
            Ok(opened) => opened,
            Err(tungstenite::Error::Http(response)) => {
                log::debug!("GET {path} {}", response.status());
                return Err(Error::Socket(format!(
                    "the app answered {}",
                    response.status()
                )));
            }
            Err(error) => {
                log::debug!("GET {path} failed: {error}");
                return Err(Error::Socket(error.to_string()));
            }
        };
        log::debug!("GET {path} {}", response.status());
        let protocol = response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|value: &HeaderValue| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();

        let (commands, queue) = mpsc::unbounded_channel();
        let closing = Arc::new(Notify::new());
        let entry = Arc::new(Socket {
            inbox: Mutex::new(Inbox::default()),
            arrived: Notify::new(),
            room: Notify::new(),
            closing: closing.clone(),
            reading: tokio::sync::Mutex::new(()),
            commands,
        });
        // An old page's socket drops here, which closes its connection.
        let id = self
            .registry
            .insert(webview, generation, entry.clone())
            .ok_or(Error::SocketClosed)?;
        let (sink, source) = socket.split();
        let (stop, stopped) = oneshot::channel();
        tokio::spawn(write(sink, queue, closing, stopped));
        tokio::spawn(read(
            source,
            entry,
            self.read_ahead,
            self.close_timeout,
            stop,
        ));
        Ok(Opened {
            id,
            protocol,
            extensions: String::new(),
        })
    }

    /// The records of socket `id` that arrived, whole, up to [`CHUNK`]:
    /// waits at most `idle` for one, and answers nothing after that.
    pub(crate) async fn read(&self, webview: &str, id: u64, idle: Duration) -> Result<Vec<u8>> {
        let socket = self
            .registry
            .get(webview, id)
            .ok_or(Error::UnknownSocket(id))?;
        let _reading = socket.reading.lock().await;
        let deadline = Instant::now() + idle;
        loop {
            match socket.take() {
                Taken::Gone => return Err(Error::SocketClosed),
                Taken::Batch { batch, last } => {
                    socket.room.notify_one();
                    if last {
                        drop(self.registry.remove(webview, id));
                    }
                    return Ok(batch);
                }
                Taken::Nothing => {}
            }
            tokio::select! {
                () = socket.arrived.notified() => {}
                () = tokio::time::sleep_until(deadline) => return Ok(Vec::new()),
            }
        }
    }

    /// Queues `messages` for socket `id`, and waits at most `wait` for them
    /// to be written.
    pub(crate) async fn send(
        &self,
        webview: &str,
        id: u64,
        messages: Vec<Message>,
        wait: Duration,
    ) -> Result<()> {
        let socket = self
            .registry
            .get(webview, id)
            .ok_or(Error::UnknownSocket(id))?;
        let (done, written) = oneshot::channel();
        socket
            .commands
            .send(Command::Send(messages, done))
            .map_err(|_| Error::SocketClosed)?;
        drop(socket);
        let _ = tokio::time::timeout(wait, written).await;
        Ok(())
    }

    /// Starts the close of socket `id`; the last record comes through
    /// [`Sockets::read`].
    pub(crate) fn close(&self, webview: &str, id: u64, frame: Option<CloseFrame>) -> Result<()> {
        let socket = self
            .registry
            .get(webview, id)
            .ok_or(Error::UnknownSocket(id))?;
        let _ = socket.commands.send(Command::Close(frame));
        Ok(())
    }

    /// Closes every socket of `webview` with 1001 (going away), and refuses
    /// the ones its current page still has opening.
    pub(crate) fn close_webview(&self, webview: &str) {
        for socket in self.registry.close_webview(webview) {
            socket.leave();
            let _ = socket.commands.send(Command::Close(Some(CloseFrame {
                code: CloseCode::Away,
                reason: "".into(),
            })));
        }
    }
}

/// Writes the queued messages in order, until the queue or the reader ends.
async fn write(
    mut sink: SplitSink<WebSocketStream<Box<dyn Upgradable>>, Message>,
    mut queue: mpsc::UnboundedReceiver<Command>,
    closing: Arc<Notify>,
    mut stopped: oneshot::Receiver<()>,
) {
    loop {
        let command = tokio::select! {
            command = queue.recv() => command,
            _ = &mut stopped => None,
        };
        match command {
            Some(Command::Send(messages, done)) => {
                for message in messages {
                    if sink.feed(message).await.is_err() {
                        break;
                    }
                }
                let _ = sink.flush().await;
                let _ = done.send(());
            }
            Some(Command::Close(frame)) => {
                let _ = sink.send(Message::Close(frame)).await;
                closing.notify_one();
            }
            None => return,
        }
    }
}

/// Reads continuously, so the socket answers pings and closes without its
/// page, until the read-ahead is full. The last record says how it ended.
async fn read(
    mut source: SplitStream<WebSocketStream<Box<dyn Upgradable>>>,
    socket: Arc<Socket>,
    read_ahead: usize,
    close_timeout: Duration,
    _stop: oneshot::Sender<()>,
) {
    let mut close: Option<Option<CloseFrame>> = None;
    let mut deadline: Option<Instant> = None;
    let last = loop {
        let room = socket.has_room(read_ahead);
        tokio::select! {
            message = source.next(), if room => match message {
                Some(Ok(Message::Text(text))) => socket.push(Record::Text(text.as_str().to_owned())),
                Some(Ok(Message::Binary(data))) => socket.push(Record::Binary(data.to_vec())),
                Some(Ok(Message::Close(frame))) => close = Some(frame),
                Some(Ok(_)) => {}
                Some(Err(error)) => break close.map_or_else(|| Record::Error(error.to_string()), Record::closing),
                None => {
                    break close.map_or_else(
                        || Record::Error("the connection closed without a close frame".to_owned()),
                        Record::closing,
                    );
                }
            },
            () = socket.room.notified(), if !room => {}
            () = socket.closing.notified(), if deadline.is_none() => {
                deadline = Some(Instant::now() + close_timeout);
            }
            () = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                break close.map_or_else(
                    || Record::Error("the closing handshake timed out".to_owned()),
                    Record::closing,
                );
            }
        }
    };
    socket.push(last);
}

#[cfg(test)]
mod tests {
    use axum::extract::ws::{self, WebSocket, WebSocketUpgrade};
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::response::{IntoResponse, Response};
    use axum::routing::any;
    use tokio::sync::mpsc::UnboundedSender;

    use super::*;
    use crate::proxy::Proxy;
    use crate::testing;

    const SHORT: Duration = Duration::from_millis(100);

    /// Greets with the headers it saw, then echoes. `close-me` makes it close
    /// with 4000 "bye".
    async fn echo(upgrade: WebSocketUpgrade, headers: HeaderMap) -> Response {
        let greeting = format!(
            "marker={:?} origin={:?} host={:?}",
            headers.get(MARKER),
            headers.get(header::ORIGIN),
            headers.get(header::HOST)
        );
        upgrade
            .protocols(["chat"])
            .on_upgrade(move |mut socket| async move {
                let _ = socket.send(ws::Message::Text(greeting.into())).await;
                while let Some(Ok(message)) = socket.recv().await {
                    match message {
                        ws::Message::Text(text) if text.as_str() == "close-me" => {
                            let frame = ws::CloseFrame {
                                code: 4000,
                                reason: "bye".into(),
                            };
                            let _ = socket.send(ws::Message::Close(Some(frame))).await;
                        }
                        ws::Message::Text(_) | ws::Message::Binary(_) => {
                            let Ok(()) = socket.send(message).await else {
                                return;
                            };
                        }
                        _ => {}
                    }
                }
            })
    }

    /// Pings, then reports what reaches it: `pong`, `close <code>`, `gone`.
    async fn watch(mut socket: WebSocket, events: UnboundedSender<String>) {
        let _ = socket.send(ws::Message::Ping("hi".into())).await;
        loop {
            match socket.recv().await {
                Some(Ok(ws::Message::Pong(_))) => {
                    let _ = events.send("pong".to_owned());
                }
                Some(Ok(ws::Message::Close(frame))) => {
                    let code = frame.map_or(0, |frame| frame.code);
                    let _ = events.send(format!("close {code}"));
                    return;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => {
                    let _ = events.send("gone".to_owned());
                    return;
                }
            }
        }
    }

    fn router(events: UnboundedSender<String>) -> axum::Router {
        axum::Router::new()
            .route("/ws", any(echo))
            .route(
                "/forbidden",
                any(|| async { StatusCode::FORBIDDEN.into_response() }),
            )
            .route(
                "/silent",
                any(|upgrade: WebSocketUpgrade| async move {
                    upgrade.on_upgrade(|socket| async move {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        drop(socket);
                    })
                }),
            )
            .route(
                "/watch",
                any(move |upgrade: WebSocketUpgrade| {
                    let events = events.clone();
                    async move { upgrade.on_upgrade(move |socket| watch(socket, events)) }
                }),
            )
            .route(
                "/flood",
                any(|upgrade: WebSocketUpgrade| async move {
                    upgrade.on_upgrade(|mut socket| async move {
                        for _ in 0..50 {
                            let _ = socket.send(ws::Message::Binary(vec![7; 4].into())).await;
                        }
                        let _ = socket.send(ws::Message::Text("done".into())).await;
                        while socket.recv().await.is_some() {}
                    })
                }),
            )
    }

    fn in_process() -> (Dispatcher, mpsc::UnboundedReceiver<String>) {
        let (events, received) = mpsc::unbounded_channel();
        (Dispatcher::Router(router(events)), received)
    }

    async fn open(
        sockets: &Sockets,
        dispatcher: &Dispatcher,
        path: &str,
        protocols: &[&str],
    ) -> Result<Opened> {
        let protocols: Vec<String> = protocols.iter().map(|&p| p.to_owned()).collect();
        let generation = sockets.generation("main");
        sockets
            .open(dispatcher, "main", generation, path, &protocols)
            .await
    }

    /// The records of a batch, as `fetch.js` decodes them.
    fn decode(mut bytes: &[u8]) -> Vec<Record> {
        let mut records = Vec::new();
        while !bytes.is_empty() {
            let kind = bytes[0];
            let length = u32::from_be_bytes(bytes[1..5].try_into().expect("4 bytes")) as usize;
            let payload = &bytes[5..5 + length];
            records.push(match kind {
                TEXT => Record::Text(String::from_utf8(payload.to_vec()).expect("utf-8")),
                BINARY => Record::Binary(payload.to_vec()),
                CLOSE => Record::Close(
                    u16::from_be_bytes([payload[0], payload[1]]),
                    String::from_utf8(payload[2..].to_vec()).expect("utf-8"),
                ),
                ERROR => Record::Error(String::from_utf8(payload.to_vec()).expect("utf-8")),
                _ => panic!("unknown kind {kind}"),
            });
            bytes = &bytes[5 + length..];
        }
        records
    }

    /// The next records of socket `id`, past idle answers.
    async fn next(sockets: &Sockets, id: u64) -> Vec<Record> {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let batch = sockets.read("main", id, SHORT).await.expect("read");
                if !batch.is_empty() {
                    return decode(&batch);
                }
            }
        })
        .await
        .expect("records in time")
    }

    async fn event(events: &mut mpsc::UnboundedReceiver<String>) -> String {
        tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("an event in time")
            .expect("the route is alive")
    }

    #[tokio::test]
    async fn sockets_in_process_carry_the_marker_and_no_origin() {
        let (dispatcher, _events) = in_process();
        let sockets = Sockets::default();
        let opened = open(&sockets, &dispatcher, "/ws", &[]).await.expect("open");
        let [Record::Text(greeting)] = &next(&sockets, opened.id).await[..] else {
            panic!("a greeting");
        };
        assert!(
            greeting.contains(&format!("marker=Some({:?})", crate::ORIGIN)),
            "{greeting}"
        );
        assert!(greeting.contains("origin=None"), "{greeting}");

        let messages = vec![Message::text("hi"), Message::binary(vec![1, 2, 3])];
        sockets
            .send("main", opened.id, messages, SHORT)
            .await
            .expect("send");
        let mut echoed = Vec::new();
        while echoed.len() < 2 {
            echoed.extend(next(&sockets, opened.id).await);
        }
        assert_eq!(
            echoed,
            [Record::Text("hi".to_owned()), Record::Binary(vec![1, 2, 3])]
        );
    }

    #[tokio::test]
    async fn the_dev_forward_names_the_upstream_host() {
        let (events, _received) = mpsc::unbounded_channel();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("bound address");
        let app = router(events);
        tokio::spawn(async move { axum::serve(listener, app).await });
        let upstream: url::Url = format!("http://{addr}").parse().expect("valid url");
        let dispatcher = Dispatcher::Proxy(Proxy::new(&upstream).expect("http upstream"));

        let sockets = Sockets::default();
        let opened = open(&sockets, &dispatcher, "/ws", &[]).await.expect("open");
        let [Record::Text(greeting)] = &next(&sockets, opened.id).await[..] else {
            panic!("a greeting");
        };
        assert!(
            greeting.contains(&format!("host=Some(\"{addr}\")")),
            "{greeting}"
        );
        assert!(greeting.contains("origin=None"), "{greeting}");
    }

    #[tokio::test]
    async fn subprotocols_are_chosen_or_refused() {
        let (dispatcher, _events) = in_process();
        let sockets = Sockets::default();
        let chosen = open(&sockets, &dispatcher, "/ws", &["chat"])
            .await
            .expect("open");
        assert_eq!(chosen.protocol, "chat");
        let refused = open(&sockets, &dispatcher, "/ws", &["other"]).await;
        assert!(matches!(refused, Err(Error::Socket(_))), "{refused:?}");
    }

    #[tokio::test]
    async fn a_refused_upgrade_fails_the_open() {
        let (dispatcher, _events) = in_process();
        let refused = open(&Sockets::default(), &dispatcher, "/forbidden", &[]).await;
        assert!(
            matches!(&refused, Err(Error::Socket(message)) if message.contains("403")),
            "{refused:?}"
        );
    }

    #[tokio::test]
    async fn the_apps_close_is_the_last_record() {
        let (dispatcher, _events) = in_process();
        let sockets = Sockets::default();
        let opened = open(&sockets, &dispatcher, "/ws", &[]).await.expect("open");
        next(&sockets, opened.id).await;
        sockets
            .send("main", opened.id, vec![Message::text("close-me")], SHORT)
            .await
            .expect("send");
        assert_eq!(
            next(&sockets, opened.id).await,
            [Record::Close(4000, "bye".to_owned())]
        );
        assert!(matches!(
            sockets.read("main", opened.id, SHORT).await,
            Err(Error::UnknownSocket(_))
        ));
    }

    #[tokio::test]
    async fn a_page_close_is_answered() {
        let (dispatcher, _events) = in_process();
        let sockets = Sockets::default();
        let opened = open(&sockets, &dispatcher, "/ws", &[]).await.expect("open");
        next(&sockets, opened.id).await;
        let frame = CloseFrame {
            code: CloseCode::Normal,
            reason: "done".into(),
        };
        sockets
            .close("main", opened.id, Some(frame))
            .expect("close");
        assert_eq!(
            next(&sockets, opened.id).await,
            [Record::Close(1000, "done".to_owned())]
        );
    }

    #[tokio::test]
    async fn a_close_without_an_answer_times_out() {
        let (dispatcher, _events) = in_process();
        let sockets = Sockets {
            close_timeout: SHORT,
            ..Sockets::default()
        };
        let opened = open(&sockets, &dispatcher, "/silent", &[])
            .await
            .expect("open");
        sockets.close("main", opened.id, None).expect("close");
        assert_eq!(
            next(&sockets, opened.id).await,
            [Record::Error("the closing handshake timed out".to_owned())]
        );
    }

    #[tokio::test]
    async fn pings_are_answered_without_a_read() {
        let (dispatcher, mut events) = in_process();
        let sockets = Sockets::default();
        open(&sockets, &dispatcher, "/watch", &[])
            .await
            .expect("open");
        assert_eq!(event(&mut events).await, "pong");
    }

    #[tokio::test]
    async fn closing_a_webview_wakes_its_read_and_goes_away() {
        let (dispatcher, mut events) = in_process();
        let sockets = Arc::new(Sockets::default());
        let opened = open(&sockets, &dispatcher, "/watch", &[])
            .await
            .expect("open");
        assert_eq!(event(&mut events).await, "pong");
        let reader = {
            let sockets = sockets.clone();
            tokio::spawn(async move {
                sockets
                    .read("main", opened.id, Duration::from_secs(10))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        sockets.close_webview("main");
        let read = tokio::time::timeout(Duration::from_secs(1), reader)
            .await
            .expect("woken")
            .expect("joined");
        assert!(matches!(read, Err(Error::SocketClosed)), "{read:?}");
        assert_eq!(event(&mut events).await, "close 1001");
    }

    #[tokio::test]
    async fn a_socket_for_a_page_that_is_gone_is_dropped() {
        let (dispatcher, mut events) = in_process();
        let sockets = Sockets::default();
        let generation = sockets.generation("main");
        sockets.close_webview("main");
        let late = sockets
            .open(&dispatcher, "main", generation, "/watch", &[])
            .await;
        assert!(matches!(late, Err(Error::SocketClosed)), "{late:?}");
        assert_eq!(event(&mut events).await, "gone");
    }

    #[tokio::test]
    async fn sockets_belong_to_their_webview() {
        let (dispatcher, _events) = in_process();
        let sockets = Sockets::default();
        let opened = open(&sockets, &dispatcher, "/ws", &[]).await.expect("open");
        assert!(matches!(
            sockets.read("other", opened.id, SHORT).await,
            Err(Error::UnknownSocket(_))
        ));
    }

    #[tokio::test]
    async fn an_idle_read_answers_nothing() {
        let (dispatcher, _events) = in_process();
        let sockets = Sockets::default();
        let opened = open(&sockets, &dispatcher, "/watch", &[])
            .await
            .expect("open");
        let batch = sockets.read("main", opened.id, SHORT).await.expect("read");
        assert!(batch.is_empty());
    }

    #[tokio::test]
    async fn the_read_ahead_bounds_what_waits_for_the_page() {
        let (dispatcher, _events) = in_process();
        let sockets = Sockets {
            read_ahead: 10,
            ..Sockets::default()
        };
        let opened = open(&sockets, &dispatcher, "/flood", &[])
            .await
            .expect("open");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let socket = sockets.registry.get("main", opened.id).expect("open");
        assert!(socket.inbox().bytes <= 10 + 4, "{}", socket.inbox().bytes);
        drop(socket);

        let mut binaries = 0;
        loop {
            match &next(&sockets, opened.id).await[..] {
                records if records.contains(&Record::Text("done".to_owned())) => {
                    binaries += records.len() - 1;
                    break;
                }
                records => binaries += records.len(),
            }
        }
        assert_eq!(binaries, 50);
    }

    /// The binary payload of a `tests/wire.json` vector.
    fn binary(vector: &serde_json::Value) -> Option<Vec<u8>> {
        let bytes = vector["binary"].as_array()?;
        Some(bytes.iter().map(testing::byte).collect())
    }

    #[test]
    fn records_have_exact_bytes() {
        for vector in testing::wire()["records"].as_array().expect("records") {
            let string = |key: &str| vector[key].as_str().map(str::to_owned);
            let record = if let Some(text) = string("text") {
                Record::Text(text)
            } else if let Some(data) = binary(vector) {
                Record::Binary(data)
            } else if let Some(close) = vector["close"].as_array() {
                let code = close[0].as_u64().and_then(|code| u16::try_from(code).ok());
                let reason = close[1].as_str().expect("a reason").to_owned();
                Record::Close(code.expect("a code"), reason)
            } else {
                Record::Error(string("error").expect("a record"))
            };
            let mut bytes = Vec::new();
            record.encode(&mut bytes);
            assert_eq!(bytes, testing::bytes(&vector["bytes"]), "{vector}");
        }
    }

    #[test]
    fn sends_decode_text_and_binary_only() {
        let wire = testing::wire();
        let sends = wire["sends"].as_array().expect("sends");
        let body: Vec<u8> = sends
            .iter()
            .flat_map(|vector| testing::bytes(&vector["bytes"]))
            .collect();
        let messages: Vec<Message> = sends
            .iter()
            .map(|vector| match vector["text"].as_str() {
                Some(text) => Message::text(text),
                None => Message::binary(binary(vector).expect("text or binary")),
            })
            .collect();
        assert_eq!(decode_sends(&body).expect("valid"), messages);
        for bad in [
            &[0, 0, 0, 0, 1, 0xff][..],
            &[0, 0, 0, 0, 5, b'h'][..],
            &[0, 0, 0][..],
            &[2, 0, 0, 0, 0][..],
        ] {
            assert!(
                matches!(decode_sends(bad), Err(Error::InvalidRequest(_))),
                "{bad:?}"
            );
        }
    }
}
