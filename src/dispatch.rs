use std::fmt::Display;

use axum::body::{Body, HttpBody};
use axum::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use axum::http::{Request, Response, StatusCode, Uri};
use http_body_util::BodyExt;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite};
use tower::ServiceExt;

use crate::proxy::Proxy;

/// Where requests for the plugin's scheme and its `fetch` command go.
pub(crate) enum Dispatcher {
    /// The app's router, in-process (release builds).
    Router(axum::Router),
    /// The `cargo leptos watch` server (dev builds).
    Proxy(Proxy),
}

/// The header on every request the plugin dispatches. Its value is the page
/// origin.
pub(crate) const MARKER: HeaderName = HeaderName::from_static("leptos-ssr-origin");

const HOP_BY_HOP: [HeaderName; 8] = [
    header::CONNECTION,
    HeaderName::from_static("keep-alive"),
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

impl Dispatcher {
    pub(crate) async fn dispatch(&self, request: Request<Body>) -> Response<Body> {
        let (mut parts, body) = request.into_parts();
        parts.uri = origin_form(&parts.uri);
        remove_all(&mut parts.headers, &HOP_BY_HOP);
        parts
            .headers
            .insert(MARKER, HeaderValue::from_static(crate::ORIGIN));
        // Neither WebKit (custom scheme) nor a JS-built `Response` (IPC)
        // decodes an encoded body.
        parts.headers.remove(header::ACCEPT_ENCODING);
        let method = parts.method.clone();
        let uri = parts.uri.clone();
        let request = Request::from_parts(parts, body);

        let mut response = match self {
            Self::Router(router) => {
                let Ok(response) = router.clone().oneshot(request).await;
                response
            }
            Self::Proxy(proxy) => proxy.forward(request).await,
        };
        remove_all(response.headers_mut(), &HOP_BY_HOP);
        log::debug!("{method} {uri} {}", response.status());
        response
    }

    /// A connection to the app for a websocket, and the host to name in its
    /// upgrade request.
    pub(crate) async fn connect(&self) -> std::io::Result<(Box<dyn Upgradable>, String)> {
        match self {
            Self::Router(router) => {
                let host = crate::ORIGIN
                    .split_once("://")
                    .map_or(crate::ORIGIN, |(_, host)| host);
                Ok((Box::new(serve_in_memory(router.clone())), host.to_owned()))
            }
            Self::Proxy(proxy) => Ok((
                Box::new(proxy.connect().await?),
                proxy.authority().to_owned(),
            )),
        }
    }

    /// The status of a body that fails after its head was sent: the app's
    /// (500) or the dev server's (502).
    pub(crate) fn body_error_status(&self) -> StatusCode {
        match self {
            Self::Router(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Proxy(_) => StatusCode::BAD_GATEWAY,
        }
    }
}

/// A stream that a websocket upgrade can run over.
pub(crate) trait Upgradable: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Upgradable for T {}

/// One end of an in-memory connection whose other end the router serves.
/// axum's `WebSocketUpgrade` takes its upgrade from a hyper connection, which
/// `oneshot` on the router cannot give it.
fn serve_in_memory(router: axum::Router) -> tokio::io::DuplexStream {
    let (client, server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let connection = hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .serve_connection(TokioIo::new(server), TowerToHyperService::new(router))
            .with_upgrades();
        if let Err(error) = connection.await {
            log::debug!("in-memory connection to the app: {error}");
        }
    });
    client
}

/// The path and query of `uri`: the scheme hands over absolute URIs.
pub(crate) fn origin_form(uri: &Uri) -> Uri {
    uri.path_and_query()
        .map_or_else(|| Uri::from_static("/"), |path| Uri::from(path.clone()))
}

/// Buffers `response`; a body that fails midway becomes a `status` error.
pub(crate) async fn collect<B>(response: Response<B>, status: StatusCode) -> Response<Vec<u8>>
where
    B: HttpBody,
    B::Error: Display,
{
    let (parts, body) = response.into_parts();
    match body.collect().await {
        Ok(collected) => Response::from_parts(parts, collected.to_bytes().to_vec()),
        Err(error) => text(status, error.to_string()),
    }
}

pub(crate) fn text(status: StatusCode, body: impl Into<String>) -> Response<Vec<u8>> {
    with_type(status, "text/plain; charset=utf-8", body.into())
}

pub(crate) fn html(status: StatusCode, body: impl Into<String>) -> Response<Vec<u8>> {
    with_type(status, "text/html; charset=utf-8", body.into())
}

fn with_type(status: StatusCode, content_type: &'static str, body: String) -> Response<Vec<u8>> {
    let mut response = Response::new(body.into_bytes());
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

/// Escapes `value` for an HTML attribute or text node.
pub(crate) fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            c => escaped.push(c),
        }
    }
    escaped
}

fn remove_all(headers: &mut HeaderMap, names: &[HeaderName]) {
    for name in names {
        headers.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use axum::routing::{get, post};

    use super::*;

    fn router() -> axum::Router {
        axum::Router::new()
            .route("/page", get(|| async { "page" }))
            .route(
                "/headers",
                get(|headers: HeaderMap| async move {
                    format!("{:?}", headers.get(header::ACCEPT_ENCODING))
                }),
            )
            .route("/echo", post(|body: String| async move { body }))
            .route("/marker", get(marker))
    }

    async fn marker(headers: HeaderMap) -> String {
        format!("{:?}", headers.get(MARKER))
    }

    fn request(method: &str, uri: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::ACCEPT_ENCODING, "gzip")
            .header(MARKER, "a page's own value")
            .body(Body::from(body.to_owned()))
            .expect("valid request")
    }

    async fn call(method: &str, uri: &str, body: &str) -> Response<Vec<u8>> {
        let response = Dispatcher::Router(router())
            .dispatch(request(method, uri, body))
            .await;
        collect(response, StatusCode::INTERNAL_SERVER_ERROR).await
    }

    #[test]
    fn scheme_uris_become_origin_form() {
        for uri in [
            "leptos://localhost/a/b?c=1",
            "http://leptos.localhost/a/b?c=1",
        ] {
            let uri: Uri = uri.parse().expect("valid uri");
            assert_eq!(origin_form(&uri), "/a/b?c=1");
        }
        let bare: Uri = "leptos://localhost".parse().expect("valid uri");
        assert_eq!(origin_form(&bare), "/");
    }

    #[tokio::test]
    async fn absolute_uris_reach_the_router() {
        let response = call("GET", "leptos://localhost/page", "").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body(), b"page");
    }

    #[tokio::test]
    async fn accept_encoding_never_reaches_the_app() {
        let response = call("GET", "leptos://localhost/headers", "").await;
        assert_eq!(response.body(), b"None");
    }

    #[tokio::test]
    async fn the_router_sees_the_marker() {
        let response = call("GET", "leptos://localhost/marker", "").await;
        assert_eq!(
            response.body(),
            format!("Some({:?})", crate::ORIGIN).as_bytes()
        );
    }

    #[tokio::test]
    async fn the_dev_server_sees_the_marker() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("bound address");
        let app = axum::Router::new().route("/marker", get(marker));
        tokio::spawn(async move { axum::serve(listener, app).await });
        let upstream: url::Url = format!("http://{addr}").parse().expect("valid url");
        let proxy = Proxy::new(&upstream).expect("http upstream");

        let response = Dispatcher::Proxy(proxy)
            .dispatch(request("GET", "leptos://localhost/marker", ""))
            .await;
        let response = collect(response, StatusCode::BAD_GATEWAY).await;
        assert_eq!(
            response.body(),
            format!("Some({:?})", crate::ORIGIN).as_bytes()
        );
    }

    #[tokio::test]
    async fn bodies_reach_the_router() {
        let response = call("POST", "leptos://localhost/echo", "a=1").await;
        assert_eq!(response.body(), b"a=1");
    }

    #[test]
    fn html_is_escaped() {
        assert_eq!(
            escape_html(r#"/a?b=1&c="<d>'"#),
            "/a?b=1&amp;c=&quot;&lt;d&gt;&#39;"
        );
    }
}
