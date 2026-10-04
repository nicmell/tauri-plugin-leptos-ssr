use std::fmt::Display;

use axum::body::{Body, HttpBody};
use axum::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use axum::http::{Request, Response, StatusCode, Uri};
use http_body_util::BodyExt;
use tower::ServiceExt;

use crate::proxy::Proxy;

/// Where requests for the plugin's scheme and its `fetch` command go.
pub(crate) enum Dispatcher {
    /// The app's router, in-process (release builds).
    Router(axum::Router),
    /// The `cargo leptos watch` server (dev builds).
    Proxy(Proxy),
}

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
    pub(crate) async fn dispatch(&self, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
        let (mut parts, body) = request.into_parts();
        parts.uri = origin_form(&parts.uri);
        remove_all(&mut parts.headers, &HOP_BY_HOP);
        // WebKit does not decode encoded bodies coming from a custom scheme.
        parts.headers.remove(header::ACCEPT_ENCODING);
        let method = parts.method.clone();
        let uri = parts.uri.clone();
        let request = Request::from_parts(parts, body);

        let mut response = match self {
            Self::Router(router) => route(router.clone(), request).await,
            Self::Proxy(proxy) => proxy.forward(request).await,
        };
        remove_all(response.headers_mut(), &HOP_BY_HOP);
        // wry on macOS copies response headers over the length it computed itself.
        response.headers_mut().remove(header::CONTENT_LENGTH);
        log::debug!("{method} {uri} {}", response.status());
        response
    }
}

async fn route(router: axum::Router, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
    let Ok(response) = router.oneshot(request.map(Body::from)).await;
    collect(response, StatusCode::INTERNAL_SERVER_ERROR).await
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
    }

    fn request(method: &str, uri: &str, body: &str) -> Request<Vec<u8>> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::ACCEPT_ENCODING, "gzip")
            .body(body.as_bytes().to_vec())
            .expect("valid request")
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
        let dispatcher = Dispatcher::Router(router());
        let response = dispatcher
            .dispatch(request("GET", "leptos://localhost/page", ""))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body(), b"page");
        assert!(response.headers().get(header::CONTENT_LENGTH).is_none());
    }

    #[tokio::test]
    async fn accept_encoding_never_reaches_the_app() {
        let dispatcher = Dispatcher::Router(router());
        let response = dispatcher
            .dispatch(request("GET", "leptos://localhost/headers", ""))
            .await;
        assert_eq!(response.body(), b"None");
    }

    #[tokio::test]
    async fn bodies_reach_the_router() {
        let dispatcher = Dispatcher::Router(router());
        let response = dispatcher
            .dispatch(request("POST", "leptos://localhost/echo", "a=1"))
            .await;
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
