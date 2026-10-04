use axum::http::header::{self, HeaderValue};
use axum::http::{Method, Request, Response, StatusCode};
use tauri::{Manager, Runtime, UriSchemeContext, UriSchemeResponder};

use crate::LeptosSsr;
use crate::dispatch::{self, Dispatcher};

/// The `leptos` scheme handler.
pub(crate) fn handle<R: Runtime>(
    ctx: UriSchemeContext<'_, R>,
    request: Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    let app = ctx.app_handle().clone();
    tauri::async_runtime::spawn(async move {
        let response = match app.try_state::<LeptosSsr>() {
            Some(state) => respond(&state.dispatcher, request).await,
            None => with_cors(dispatch::text(
                StatusCode::SERVICE_UNAVAILABLE,
                "the leptos-ssr plugin is not set up",
            )),
        };
        responder.respond(response);
    });
}

/// Answers one scheme request: GET and HEAD only, redirects as pages.
pub(crate) async fn respond(
    dispatcher: &Dispatcher,
    request: Request<Vec<u8>>,
) -> Response<Vec<u8>> {
    let response = if matches!(*request.method(), Method::GET | Method::HEAD) {
        redirect_as_page(dispatcher.dispatch(request).await)
    } else {
        method_not_allowed()
    };
    with_cors(response)
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

    fn dispatcher() -> Dispatcher {
        Dispatcher::Router(
            axum::Router::new()
                .route("/", get(|| async { "home" }))
                .route("/old", get(|| async { Redirect::to("/new?a=1&b=2") })),
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
        let response = respond(&dispatcher(), request(Method::GET, "leptos://localhost/")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body(), b"home");
        assert_eq!(allow_origin(&response), Some(crate::ORIGIN));
    }

    #[tokio::test]
    async fn head_is_dispatched_without_a_body() {
        let response = respond(&dispatcher(), request(Method::HEAD, "leptos://localhost/")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.body().is_empty());
    }

    #[tokio::test]
    async fn methods_with_a_body_are_refused() {
        let response = respond(&dispatcher(), request(Method::POST, "leptos://localhost/")).await;
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
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(response.body().clone()).expect("utf-8 page");
        assert!(body.contains(r#"content="0;url=/new?a=1&amp;b=2""#));
        assert_eq!(allow_origin(&response), Some(crate::ORIGIN));
    }
}
