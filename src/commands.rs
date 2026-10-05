use axum::body::Body;
use axum::http::header::{HeaderMap, HeaderName, HeaderValue};
use axum::http::response::Parts;
use axum::http::{Method, Request};
use percent_encoding::percent_decode_str;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tauri::ipc::{self, InvokeBody};
use tauri::{Manager, Runtime, Webview, command};

use crate::streams::{self, Chunk};
use crate::{Error, LeptosSsr, Result};

/// The IPC header carrying the method, URL and headers of a `fetch` request:
/// Tauri sets `Content-Type` on the IPC request itself.
const REQUEST_HEADER: &str = "leptos-ssr-request";

#[derive(Deserialize)]
struct RequestHead {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
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
    let request = http_request(request.headers(), request.body())?;
    let state = webview.state::<LeptosSsr>();
    let label = webview.label();
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

/// The request `fetch.js` sent: its head from [`REQUEST_HEADER`], its body
/// from [`raw_body`].
fn http_request(headers: &HeaderMap, body: &InvokeBody) -> Result<Request<Body>> {
    let head: RequestHead = head(headers)?;
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

    async fn body_text(request: Request<Body>) -> String {
        let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
            .await
            .expect("body");
        String::from_utf8(bytes.to_vec()).expect("utf-8")
    }

    #[tokio::test]
    async fn requests_come_raw_or_as_number_arrays() {
        let headers = ipc_headers("leptos://localhost/api/greet?x=1");
        for body in [
            InvokeBody::Raw(b"a=1".to_vec()),
            InvokeBody::Json(serde_json::json!([97, 61, 49])),
        ] {
            let request = http_request(&headers, &body).expect("valid request");
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
            http_request(&HeaderMap::new(), &InvokeBody::Raw(Vec::new())),
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
                    http_request(&ipc_headers(url), &InvokeBody::Raw(Vec::new())),
                    Err(Error::ForeignUrl(_))
                ),
                "{url}"
            );
        }
        assert!(
            http_request(
                &ipc_headers("http://leptos.localhost/api"),
                &InvokeBody::Raw(Vec::new())
            )
            .is_ok()
        );
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
    fn frames_carry_head_and_initial_bytes() {
        let mut response = Response::new(());
        *response.status_mut() = StatusCode::CREATED;
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        let (parts, ()) = response.into_parts();

        let frame = frame(&parts, Some(7), b"body").expect("frame");
        let length = u32::from_be_bytes(frame[..4].try_into().expect("4 bytes")) as usize;
        let head: serde_json::Value =
            serde_json::from_slice(&frame[4..4 + length]).expect("json head");
        assert_eq!(head["status"], 201);
        assert_eq!(head["headers"][0][0], "content-type");
        assert_eq!(head["headers"][0][1], "text/plain");
        assert_eq!(head["id"], 7);
        assert_eq!(&frame[4 + length..], b"body");
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
        let request = http_request(
            &ipc_headers("leptos://localhost/echo"),
            &InvokeBody::Raw(b"a=1".to_vec()),
        )
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
