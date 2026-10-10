use axum::body::Body;
use axum::http::header::{HeaderMap, HeaderName, HeaderValue};
use axum::http::response::Parts;
use axum::http::{Method, Request};
use percent_encoding::percent_decode_str;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tauri::ipc::{self, InvokeBody};
use tauri::{Manager, Runtime, Webview, command};

use crate::calls::{Calls, Running};
use crate::sockets::{self, Opened};
use crate::streams::{self, Chunk};
use crate::{Error, LeptosSsr, Result};

/// The IPC header carrying the head of a request with a body (`fetch`'s
/// method, URL, headers and call, or `ws_send`'s socket): Tauri sets
/// `Content-Type` on the IPC request itself.
const REQUEST_HEADER: &str = "leptos-ssr-request";

#[derive(Deserialize)]
struct RequestHead {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    call: String,
}

#[derive(Deserialize)]
struct SendHead {
    id: u64,
}

#[derive(Serialize)]
struct ResponseHead<'a> {
    status: u16,
    headers: Vec<(&'a str, &'a str)>,
    id: Option<u64>,
}

/// Dispatches a request from `fetch.js` like the scheme does and answers with
/// [`frame`]; the rest of a streamed body comes from [`fetch_read_body`].
#[command]
pub(crate) async fn fetch<R: Runtime>(
    webview: Webview<R>,
    request: ipc::Request<'_>,
) -> Result<ipc::Response> {
    let head: RequestHead = head(request.headers())?;
    let state = webview.state::<LeptosSsr>();
    let label = webview.label();
    let _call = start(&state.calls, label, &head.call)?;
    let request = http_request(head, request.body())?;
    let generation = state.streams.generation(label);
    let response = state.dispatcher.dispatch(request).await;
    let (parts, initial, id) = state.streams.start(label, generation, response).await?;
    Ok(ipc::Response::new(frame(&parts, id, &initial)?))
}

/// The next chunk of a streamed body, as [`Chunk::into_bytes`] encodes it.
#[command]
pub(crate) async fn fetch_read_body<R: Runtime>(
    webview: Webview<R>,
    id: u64,
) -> Result<ipc::Response> {
    let state = webview.state::<LeptosSsr>();
    let chunk = state
        .streams
        .read(webview.label(), id, streams::IDLE)
        .await?;
    Ok(ipc::Response::new(Chunk::into_bytes(chunk)))
}

/// Drops a streamed body the page no longer reads.
#[command]
pub(crate) fn fetch_cancel_body<R: Runtime>(webview: Webview<R>, id: u64) {
    if let Some(state) = webview.try_state::<LeptosSsr>() {
        state.streams.cancel(webview.label(), id);
    }
}

/// Opens a websocket to `url`, on the plugin's own origin only, within
/// [`streams::IDLE`].
#[command]
pub(crate) async fn ws_open<R: Runtime>(
    webview: Webview<R>,
    url: String,
    protocols: Vec<String>,
    call: String,
) -> Result<Opened> {
    let path = own_path(&url)?;
    if let Some(bad) = protocols.iter().find(|protocol| !is_token(protocol)) {
        return Err(invalid(format!("`{bad}` is not a subprotocol")));
    }
    let state = webview.state::<LeptosSsr>();
    let label = webview.label();
    let _call = start(&state.calls, label, &call)?;
    let generation = state.sockets.generation(label);
    let opening = state
        .sockets
        .open(&state.dispatcher, label, generation, &path, &protocols);
    tokio::time::timeout(streams::IDLE, opening)
        .await
        .map_err(|_| Error::Socket(format!("{path} did not answer the upgrade")))?
}

/// The records of a websocket that arrived, as [`sockets::Record::encode`]
/// writes them; none within [`streams::IDLE`] answers an empty batch.
#[command]
pub(crate) async fn ws_read<R: Runtime>(webview: Webview<R>, id: u64) -> Result<ipc::Response> {
    let state = webview.state::<LeptosSsr>();
    let batch = state
        .sockets
        .read(webview.label(), id, streams::IDLE)
        .await?;
    Ok(ipc::Response::new(batch))
}

/// Writes the records of the body to a websocket, whose id travels in
/// [`REQUEST_HEADER`].
#[command]
pub(crate) async fn ws_send<R: Runtime>(
    webview: Webview<R>,
    request: ipc::Request<'_>,
) -> Result<()> {
    let head: SendHead = head(request.headers())?;
    let messages = sockets::decode_sends(&raw_body(request.body())?)?;
    let state = webview.state::<LeptosSsr>();
    state
        .sockets
        .send(webview.label(), head.id, messages, streams::IDLE)
        .await
}

/// Starts the close of a websocket: `code` is 1000 or 3000 to 4999, and
/// `reason` at most 123 bytes.
#[command]
pub(crate) fn ws_close<R: Runtime>(
    webview: Webview<R>,
    id: u64,
    code: Option<u16>,
    reason: Option<String>,
) -> Result<()> {
    let frame = close_frame(code, reason)?;
    match webview.try_state::<LeptosSsr>() {
        Some(state) => state.sockets.close(webview.label(), id, frame),
        None => Ok(()),
    }
}

fn close_frame(code: Option<u16>, reason: Option<String>) -> Result<Option<sockets::CloseFrame>> {
    let reason = reason.unwrap_or_default();
    if reason.len() > 123 {
        return Err(invalid("a close reason is at most 123 bytes"));
    }
    match code {
        Some(code @ (1000 | 3000..=4999)) => Ok(Some(sockets::CloseFrame {
            code: code.into(),
            reason: reason.into(),
        })),
        Some(code) => Err(invalid(format!("{code} is not a close code a page sends"))),
        None if reason.is_empty() => Ok(None),
        None => Err(invalid("a close reason needs a code")),
    }
}

/// Starts `call` of `webview`, unless it ran already.
fn start<'a>(calls: &'a Calls, webview: &str, call: &str) -> Result<Running<'a>> {
    if call.len() > 64 {
        return Err(invalid("a call id is at most 64 bytes"));
    }
    calls
        .start(webview, call)
        .ok_or_else(|| Error::Repeated(call.to_owned()))
}

/// An RFC 7230 token, as a subprotocol must be.
fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

/// The request `fetch.js` sent: `head` from [`REQUEST_HEADER`], its body from
/// [`raw_body`].
fn http_request(head: RequestHead, body: &InvokeBody) -> Result<Request<Body>> {
    let path = own_path(&head.url)?;
    let mut builder = Request::builder()
        .method(Method::from_bytes(head.method.as_bytes()).map_err(invalid)?)
        .uri(path);
    for (name, value) in &head.headers {
        builder = builder.header(
            HeaderName::from_bytes(name.as_bytes()).map_err(invalid)?,
            HeaderValue::from_str(value).map_err(invalid)?,
        );
    }
    builder.body(Body::from(raw_body(body)?)).map_err(invalid)
}

/// The JSON head in [`REQUEST_HEADER`], percent-encoded.
fn head<T: DeserializeOwned>(headers: &HeaderMap) -> Result<T> {
    let head = headers
        .get(REQUEST_HEADER)
        .ok_or_else(|| invalid(format!("missing `{REQUEST_HEADER}` header")))?;
    let head = percent_decode_str(head.to_str().map_err(invalid)?)
        .decode_utf8()
        .map_err(invalid)?;
    serde_json::from_str(&head).map_err(invalid)
}

/// A request body: raw (custom-protocol IPC) or a number array (postMessage
/// IPC).
fn raw_body(body: &InvokeBody) -> Result<Vec<u8>> {
    match body {
        InvokeBody::Raw(bytes) => Ok(bytes.clone()),
        InvokeBody::Json(value) => Vec::<u8>::deserialize(value).map_err(invalid),
    }
}

/// The path and query of `url` when it is on the plugin's origin:
/// `leptos://localhost`, or `leptos.localhost` over http, https, ws or wss,
/// without a port.
fn own_path(url: &str) -> Result<String> {
    let parsed = url::Url::parse(url)?;
    let own = match parsed.scheme() {
        "leptos" => parsed.host_str() == Some("localhost"),
        "http" | "https" | "ws" | "wss" => parsed.host_str() == Some("leptos.localhost"),
        _ => false,
    };
    if !own || parsed.port().is_some() {
        return Err(Error::ForeignUrl(url.to_owned()));
    }
    Ok(parsed[url::Position::BeforePath..url::Position::AfterQuery].to_owned())
}

fn invalid(error: impl std::fmt::Display) -> Error {
    Error::InvalidRequest(error.to_string())
}

/// A response head as `fetch.js` decodes it: the length of the JSON head as a
/// big-endian u32, the head (`status`, `headers`, and the stream `id` when
/// the body continues), then the bytes already available.
fn frame(parts: &Parts, id: Option<u64>, initial: &[u8]) -> Result<Vec<u8>> {
    let head = ResponseHead {
        status: parts.status.as_u16(),
        headers: parts
            .headers
            .iter()
            .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?)))
            .collect(),
        id,
    };
    let head = serde_json::to_vec(&head).map_err(invalid)?;
    let length = u32::try_from(head.len()).map_err(invalid)?;
    let mut frame = Vec::with_capacity(4 + head.len() + initial.len());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&head);
    frame.extend_from_slice(initial);
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::Bytes;
    use axum::http::{Response, StatusCode, header};
    use axum::routing::{get, post};
    use tokio::sync::mpsc;

    use super::*;
    use crate::dispatch::Dispatcher;
    use crate::proxy::Proxy;
    use crate::streams::Streams;
    use crate::testing;

    fn ipc_headers(url: &str) -> HeaderMap {
        let head = serde_json::json!({
            "method": "POST",
            "url": url,
            "headers": [["content-type", "application/x-www-form-urlencoded"]],
            "call": "p.1",
        });
        let encoded = percent_encoding::utf8_percent_encode(
            &head.to_string(),
            percent_encoding::NON_ALPHANUMERIC,
        )
        .to_string();
        let mut headers = HeaderMap::new();
        headers.insert(
            REQUEST_HEADER,
            HeaderValue::from_str(&encoded).expect("ascii"),
        );
        headers
    }

    /// The request that `fetch.js` sends for `url` with `body`.
    fn ipc_request(url: &str, body: &InvokeBody) -> Result<Request<Body>> {
        http_request(head(&ipc_headers(url))?, body)
    }

    async fn body_text(request: Request<Body>) -> String {
        let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body");
        String::from_utf8(bytes.to_vec()).expect("utf-8")
    }

    #[tokio::test]
    async fn requests_come_raw_or_as_number_arrays() {
        for body in [
            InvokeBody::Raw(b"a=1".to_vec()),
            InvokeBody::Json(serde_json::json!([97, 61, 49])),
        ] {
            let request =
                ipc_request("leptos://localhost/api/greet?x=1", &body).expect("valid request");
            assert_eq!(request.method(), Method::POST);
            assert_eq!(request.uri(), "/api/greet?x=1");
            assert_eq!(
                request.headers()[header::CONTENT_TYPE],
                "application/x-www-form-urlencoded"
            );
            assert_eq!(body_text(request).await, "a=1");
        }
    }

    #[test]
    fn requests_need_their_head() {
        assert!(matches!(
            head::<RequestHead>(&HeaderMap::new()),
            Err(Error::InvalidRequest(_))
        ));
    }

    #[test]
    fn a_call_runs_once_under_a_short_id() {
        let calls = Calls::default();
        let _running = start(&calls, "main", "p.1").expect("a new call");
        assert!(matches!(
            start(&calls, "main", "p.1"),
            Err(Error::Repeated(_))
        ));
        assert!(matches!(
            start(&calls, "main", &"x".repeat(65)),
            Err(Error::InvalidRequest(_))
        ));
    }

    #[test]
    fn foreign_urls_are_refused() {
        for url in [
            "ipc://localhost/plugin%3Aleptos-ssr%7Cfetch",
            "http://ipc.localhost/x",
            "https://example.com/api",
            "http://leptos.localhost:8080/api",
        ] {
            assert!(
                matches!(
                    ipc_request(url, &InvokeBody::Raw(Vec::new())),
                    Err(Error::ForeignUrl(_))
                ),
                "{url}"
            );
        }
        assert!(ipc_request("http://leptos.localhost/api", &InvokeBody::Raw(Vec::new())).is_ok());
    }

    #[test]
    fn own_paths_cover_the_origin_in_every_scheme() {
        for url in [
            "leptos://localhost/ws?x=1",
            "http://leptos.localhost/ws?x=1",
            "https://leptos.localhost/ws?x=1",
            "ws://leptos.localhost/ws?x=1",
            "wss://leptos.localhost/ws?x=1",
        ] {
            assert_eq!(own_path(url).expect(url), "/ws?x=1");
        }
        for url in [
            "ws://localhost/ws",
            "ws://leptos.localhost:3001/live_reload",
            "wss://example.com/ws",
            "ipc://localhost/x",
        ] {
            assert!(matches!(own_path(url), Err(Error::ForeignUrl(_))), "{url}");
        }
    }

    #[test]
    fn pages_close_with_their_own_codes_only() {
        assert!(matches!(close_frame(None, None), Ok(None)));
        for code in [1000, 3000, 4999] {
            let frame = close_frame(Some(code), Some("bye".to_owned()))
                .expect("a page code")
                .expect("a frame");
            assert_eq!(u16::from(frame.code), code);
            assert_eq!(frame.reason.as_str(), "bye");
        }
        for (code, reason) in [
            (Some(1001), None),
            (Some(2999), None),
            (Some(5000), None),
            (Some(1000), Some("x".repeat(124))),
            (None, Some("no code".to_owned())),
        ] {
            assert!(
                matches!(close_frame(code, reason), Err(Error::InvalidRequest(_))),
                "{code:?}"
            );
        }
    }

    #[test]
    fn subprotocols_are_tokens() {
        assert!(is_token("chat.v2"));
        assert!(!is_token(""));
        assert!(!is_token("a b"));
        assert!(!is_token("a,b"));
    }

    #[test]
    fn socket_sends_come_raw_or_as_number_arrays() {
        let record = [0, 0, 0, 0, 2, b'h', b'i'];
        for body in [
            InvokeBody::Raw(record.to_vec()),
            InvokeBody::Json(serde_json::json!(record)),
        ] {
            let messages =
                sockets::decode_sends(&raw_body(&body).expect("a body")).expect("records");
            assert_eq!(
                messages,
                [tokio_tungstenite::tungstenite::Message::text("hi")]
            );
        }
    }

    #[test]
    fn frames_carry_head_and_initial_bytes() {
        let vector = &testing::wire()["frame"];
        let text = |value: &serde_json::Value| value.as_str().expect("text").to_owned();
        let mut response = Response::new(());
        *response.status_mut() = vector["status"]
            .as_u64()
            .and_then(|status| StatusCode::from_u16(u16::try_from(status).ok()?).ok())
            .expect("a status");
        for pair in vector["headers"].as_array().expect("headers") {
            response.headers_mut().insert(
                HeaderName::from_bytes(text(&pair[0]).as_bytes()).expect("a name"),
                HeaderValue::from_str(&text(&pair[1])).expect("a value"),
            );
        }
        let (parts, ()) = response.into_parts();

        let body = text(&vector["body"]);
        let frame = frame(&parts, vector["id"].as_u64(), body.as_bytes()).expect("frame");
        assert_eq!(frame, testing::bytes(&vector["bytes"]));
    }

    /// A route answering its one request with `body`.
    fn streaming_route(body: Body) -> axum::Router {
        let body = std::sync::Arc::new(std::sync::Mutex::new(Some(body)));
        axum::Router::new().route(
            "/stream",
            get(move || {
                let body = body.lock().expect("lock").take().expect("one request");
                async move { body }
            }),
        )
    }

    async fn stream_through(dispatcher: &Dispatcher, tx: &mpsc::UnboundedSender<Bytes>) {
        let streams = Streams::default();
        let request = Request::get("leptos://localhost/stream")
            .body(Body::empty())
            .expect("valid request");
        let response = dispatcher.dispatch(request).await;
        let (_, initial, id) = streams.start("main", 0, response).await.expect("started");
        assert!(initial.is_empty());
        let id = id.expect("streaming");

        tx.send(Bytes::from_static(b"tick 1"))
            .expect("receiver alive");
        let mut received = Vec::new();
        while received.is_empty() {
            match streams
                .read("main", id, Duration::from_millis(500))
                .await
                .expect("read")
            {
                Chunk::Data(data) => received = data,
                Chunk::Idle => {}
                Chunk::Last(data) => panic!("ended early with {data:?}"),
            }
        }
        assert_eq!(received, b"tick 1");
        streams.cancel("main", id);
    }

    #[tokio::test]
    async fn the_router_streams_before_the_body_ends() {
        let (tx, body, _) = testing::channel();
        let dispatcher = Dispatcher::Router(streaming_route(body));
        stream_through(&dispatcher, &tx).await;
    }

    #[tokio::test]
    async fn the_dev_proxy_streams_before_the_body_ends() {
        let (tx, body, _) = testing::channel();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("bound address");
        let app = streaming_route(body);
        tokio::spawn(async move { axum::serve(listener, app).await });
        let upstream: url::Url = format!("http://{addr}").parse().expect("valid url");
        let dispatcher = Dispatcher::Proxy(Proxy::new(&upstream).expect("http upstream"));
        stream_through(&dispatcher, &tx).await;
    }

    #[tokio::test]
    async fn proxied_requests_keep_their_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("bound address");
        let app = axum::Router::new().route("/echo", post(|body: String| async move { body }));
        tokio::spawn(async move { axum::serve(listener, app).await });

        let upstream: url::Url = format!("http://{addr}").parse().expect("valid url");
        let dispatcher = Dispatcher::Proxy(Proxy::new(&upstream).expect("http upstream"));
        let request = ipc_request("leptos://localhost/echo", &InvokeBody::Raw(b"a=1".to_vec()))
            .expect("valid request");
        let response = dispatcher.dispatch(request).await;
        let (parts, initial, id) = Streams::default()
            .start("main", 0, response)
            .await
            .expect("started");
        assert_eq!(parts.status, StatusCode::OK);
        assert_eq!(initial, b"a=1");
        assert_eq!(id, None);
    }
}
