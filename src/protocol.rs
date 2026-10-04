use std::time::Duration;

use axum::body::Body;
use axum::http::header::{self, HeaderValue};
use axum::http::{Method, Request, Response, StatusCode};
use tauri::{Manager, Runtime, UriSchemeContext, UriSchemeResponder};

use crate::LeptosSsr;
use crate::dispatch::{self, Dispatcher};

// Android abandons a custom-protocol request whose response is not complete
// after 30 s.
const TIMEOUT: Duration = Duration::from_secs(20);

/// The `leptos` scheme handler.
pub(crate) fn handle<R: Runtime>(
    ctx: UriSchemeContext<'_, R>,
    request: Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    let app = ctx.app_handle().clone();
    tauri::async_runtime::spawn(async move {
        let response = match app.try_state::<LeptosSsr>() {
            Some(state) => respond(&state.dispatcher, request, TIMEOUT).await,
            None => with_cors(dispatch::text(
                StatusCode::SERVICE_UNAVAILABLE,
                "the leptos-ssr plugin is not set up",
            )),
        };
        responder.respond(response);
    });
}

/// Answers one scheme request: GET and HEAD only, buffered within `timeout`,
/// redirects as pages.
pub(crate) async fn respond(
    dispatcher: &Dispatcher,
    request: Request<Vec<u8>>,
    timeout: Duration,
) -> Response<Vec<u8>> {
    let method = request.method().clone();
    let response = if matches!(method, Method::GET | Method::HEAD) {
        let buffered = async {
            let response = dispatcher.dispatch(request.map(Body::from)).await;
            dispatch::collect(response, dispatcher.body_error_status()).await
        };
        match tokio::time::timeout(timeout, buffered).await {
            Ok(response) => redirect_as_page(response),
            Err(_) => timed_out(dispatcher, &method),
        }
    } else {
        method_not_allowed()
    };
    let mut response = with_cors(response);
    // wry on macOS copies response headers over the length it computed itself.
    response.headers_mut().remove(header::CONTENT_LENGTH);
    response
}

fn timed_out(dispatcher: &Dispatcher, method: &Method) -> Response<Vec<u8>> {
    match dispatcher {
        Dispatcher::Proxy(proxy) => proxy.unavailable(method, "no complete answer in time"),
        Dispatcher::Router(_) => dispatch::text(
            StatusCode::GATEWAY_TIMEOUT,
            "the app did not answer in time",
        ),
    }
}

// Android custom-protocol requests carry no body, so the scheme refuses
// every method that has one, on every platform.
fn method_not_allowed() -> Response<Vec<u8>> {
    let mut response = dispatch::text(
        StatusCode::METHOD_NOT_ALLOWED,
        "the leptos scheme serves GET and HEAD; requests with a body go through `fetch`",
    );
    response
        .headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
    response
}

// wry on Android drops 3xx responses and WebKit does not follow them from a
// custom scheme, so a redirect becomes a page that navigates.
fn redirect_as_page(response: Response<Vec<u8>>) -> Response<Vec<u8>> {
    if !response.status().is_redirection() {
        return response;
    }
    let Some(location) = response
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
    else {
        return response;
    };
    let location = dispatch::escape_html(location);
    dispatch::html(
        StatusCode::OK,
        format!(
            "<!DOCTYPE html><meta http-equiv=\"refresh\" content=\"0;url={location}\">\
             <a href=\"{location}\">{location}</a>"
        ),
    )
}

fn with_cors(mut response: Response<Vec<u8>>) -> Response<Vec<u8>> {
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static(crate::ORIGIN),
    );
    response
}

#[cfg(test)]
mod tests {
    use axum::response::Redirect;
    use axum::routing::get;

    use super::*;

    const SHORT: Duration = Duration::from_millis(200);

    fn dispatcher() -> Dispatcher {
        Dispatcher::Router(
            axum::Router::new()
                .route("/", get(|| async { "home" }))
                .route("/old", get(|| async { Redirect::to("/new?a=1&b=2") }))
                .route(
                    "/sized",
                    get(|| async { ([(header::CONTENT_LENGTH, "5")], "sized") }),
                )
                .route(
                    "/slow",
                    get(|| async {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        "late"
                    }),
                ),
        )
    }

    fn request(method: Method, uri: &str) -> Request<Vec<u8>> {
        Request::builder()
            .method(method)
            .uri(uri)
            .body(Vec::new())
            .expect("valid request")
    }

    fn allow_origin(response: &Response<Vec<u8>>) -> Option<&str> {
        response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|value| value.to_str().ok())
    }

    #[tokio::test]
    async fn get_is_dispatched() {
        let response = respond(
            &dispatcher(),
            request(Method::GET, "leptos://localhost/"),
            SHORT,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body(), b"home");
        assert_eq!(allow_origin(&response), Some(crate::ORIGIN));
    }

    #[tokio::test]
    async fn head_is_dispatched_without_a_body() {
        let response = respond(
            &dispatcher(),
            request(Method::HEAD, "leptos://localhost/"),
            SHORT,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.body().is_empty());
    }

    #[tokio::test]
    async fn content_length_is_dropped() {
        let response = respond(
            &dispatcher(),
            request(Method::GET, "leptos://localhost/sized"),
            SHORT,
        )
        .await;
        assert_eq!(response.body(), b"sized");
        assert!(response.headers().get(header::CONTENT_LENGTH).is_none());
    }

    #[tokio::test]
    async fn methods_with_a_body_are_refused() {
        let response = respond(
            &dispatcher(),
            request(Method::POST, "leptos://localhost/"),
            SHORT,
        )
        .await;
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            response
                .headers()
                .get(header::ALLOW)
                .map(HeaderValue::as_bytes),
            Some(&b"GET, HEAD"[..])
        );
        assert_eq!(allow_origin(&response), Some(crate::ORIGIN));
    }

    #[tokio::test]
    async fn redirects_become_pages() {
        let response = respond(
            &dispatcher(),
            request(Method::GET, "leptos://localhost/old"),
            SHORT,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response.body().clone()).expect("utf-8 page");
        assert!(body.contains(r#"content="0;url=/new?a=1&amp;b=2""#));
        assert_eq!(allow_origin(&response), Some(crate::ORIGIN));
    }

    #[tokio::test]
    async fn a_silent_dev_server_answers_a_retrying_page() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("bound address");
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let upstream: url::Url = format!("http://{addr}").parse().expect("valid url");
        let dispatcher =
            Dispatcher::Proxy(crate::proxy::Proxy::new(&upstream).expect("http upstream"));
        let response = respond(
            &dispatcher,
            request(Method::GET, "leptos://localhost/"),
            SHORT,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = String::from_utf8(response.body().clone()).expect("utf-8 page");
        assert!(body.contains(r#"http-equiv="refresh" content="1""#));
    }

    #[tokio::test]
    async fn slow_answers_time_out() {
        let response = respond(
            &dispatcher(),
            request(Method::GET, "leptos://localhost/slow"),
            SHORT,
        )
        .await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(allow_origin(&response), Some(crate::ORIGIN));
    }
}
