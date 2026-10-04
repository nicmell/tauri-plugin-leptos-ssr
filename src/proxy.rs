use std::time::Duration;

use axum::body::Body;
use axum::http::header::{self, HeaderValue};
use axum::http::uri::{Authority, Scheme};
use axum::http::{Method, Request, Response, StatusCode, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use crate::dispatch;
use crate::{Error, Result};

// Android abandons a custom-protocol request after 30 s.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Forwards requests to the `cargo leptos watch` server.
pub(crate) struct Proxy {
    authority: Authority,
    client: Client<HttpConnector, Body>,
}

impl Proxy {
    pub(crate) fn new(upstream: &url::Url) -> Result<Self> {
        if upstream.scheme() != "http" {
            return Err(Error::DevUrlNotHttp(upstream.to_string()));
        }
        let authority = upstream[url::Position::BeforeHost..url::Position::AfterPort]
            .parse()
            .map_err(|_| Error::DevUrlNotHttp(upstream.to_string()))?;
        Ok(Self {
            authority,
            client: Client::builder(TokioExecutor::new()).build_http(),
        })
    }

    pub(crate) async fn forward(&self, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
        let (mut parts, body) = request.into_parts();
        let method = parts.method.clone();
        let mut uri = parts.uri.into_parts();
        uri.scheme = Some(Scheme::HTTP);
        uri.authority = Some(self.authority.clone());
        parts.uri = match Uri::from_parts(uri) {
            Ok(uri) => uri,
            Err(error) => return dispatch::text(StatusCode::BAD_REQUEST, error.to_string()),
        };
        if let Ok(host) = HeaderValue::from_str(self.authority.as_str()) {
            parts.headers.insert(header::HOST, host);
        }
        // A 304 would reach the webview without the body it validates.
        parts.headers.remove(header::IF_NONE_MATCH);
        parts.headers.remove(header::IF_MODIFIED_SINCE);

        let request = Request::from_parts(parts, Body::from(body));
        match tokio::time::timeout(TIMEOUT, self.client.request(request)).await {
            Ok(Ok(response)) => dispatch::collect(response, StatusCode::BAD_GATEWAY).await,
            Ok(Err(error)) => self.unavailable(&method, &error.to_string()),
            Err(_) => self.unavailable(&method, "the request timed out"),
        }
    }

    // A page retried every second, so the window recovers on its own once the
    // watch server is up again.
    fn unavailable(&self, method: &Method, reason: &str) -> Response<Vec<u8>> {
        let message = format!("waiting for the dev server at {}: {reason}", self.authority);
        if method == Method::GET {
            dispatch::html(
                StatusCode::BAD_GATEWAY,
                format!(
                    "<!DOCTYPE html><meta http-equiv=\"refresh\" content=\"1\"><p>{}</p>",
                    dispatch::escape_html(&message)
                ),
            )
        } else {
            dispatch::text(StatusCode::BAD_GATEWAY, message)
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;
    use axum::routing::get;

    use super::*;

    async fn upstream() -> url::Url {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("bound address");
        let app = axum::Router::new().route(
            "/page",
            get(|headers: HeaderMap| async move {
                format!(
                    "host={:?} if-none-match={:?}",
                    headers.get(header::HOST),
                    headers.get(header::IF_NONE_MATCH)
                )
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await });
        format!("http://{addr}").parse().expect("valid url")
    }

    fn get_request(uri: &str) -> Request<Vec<u8>> {
        Request::builder()
            .uri(uri)
            .header(header::IF_NONE_MATCH, "\"etag\"")
            .body(Vec::new())
            .expect("valid request")
    }

    #[tokio::test]
    async fn get_reaches_the_upstream() {
        let upstream = upstream().await;
        let proxy = Proxy::new(&upstream).expect("http upstream");
        let response = proxy.forward(get_request("/page")).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response.body().clone()).expect("utf-8 body");
        let host = upstream.host_str().expect("host");
        assert!(body.contains(&format!("host=Some(\"{host}:")), "{body}");
        assert!(body.contains("if-none-match=None"), "{body}");
    }

    #[tokio::test]
    async fn a_missing_upstream_answers_a_retrying_page() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("bound address");
        drop(listener);
        let upstream: url::Url = format!("http://{addr}").parse().expect("valid url");
        let response = Proxy::new(&upstream)
            .expect("http upstream")
            .forward(get_request("/page"))
            .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = String::from_utf8(response.body().clone()).expect("utf-8 body");
        assert!(body.contains(r#"http-equiv="refresh" content="1""#));
    }

    #[test]
    fn https_upstreams_are_refused() {
        let upstream: url::Url = "https://127.0.0.1:3000".parse().expect("valid url");
        assert!(matches!(
            Proxy::new(&upstream),
            Err(Error::DevUrlNotHttp(_))
        ));
    }
}
