use axum::http::header::{HeaderName, HeaderValue};
use axum::http::{Method, Request, Response};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Runtime, command, ipc};

use crate::{Error, LeptosSsr, Result};

/// A request with a body, sent by `fetch.js` from one of the plugin's pages.
#[derive(Deserialize)]
pub(crate) struct FetchRequest {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

#[derive(Serialize)]
struct Head<'a> {
    status: u16,
    headers: Vec<(&'a str, &'a str)>,
}

/// Dispatches `request` like the scheme does and answers with [`frame`].
#[command]
pub(crate) async fn fetch<R: Runtime>(
    app: AppHandle<R>,
    request: FetchRequest,
) -> Result<ipc::Response> {
    let request = request.into_http()?;
    let response = app.state::<LeptosSsr>().dispatcher.dispatch(request).await;
    Ok(ipc::Response::new(frame(&response)?))
}

impl FetchRequest {
    fn into_http(self) -> Result<Request<Vec<u8>>> {
        let url = url::Url::parse(&self.url)?;
        let own = match url.scheme() {
            "leptos" => url.host_str() == Some("localhost"),
            "http" | "https" => url.host_str() == Some("leptos.localhost"),
            _ => false,
        };
        if !own || url.port().is_some() {
            return Err(Error::ForeignUrl(self.url));
        }
        let path = &url[url::Position::BeforePath..url::Position::AfterQuery];
        let mut builder = Request::builder()
            .method(Method::from_bytes(self.method.as_bytes()).map_err(invalid)?)
            .uri(path);
        for (name, value) in &self.headers {
            builder = builder.header(
                HeaderName::from_bytes(name.as_bytes()).map_err(invalid)?,
                HeaderValue::from_str(value).map_err(invalid)?,
            );
        }
        builder.body(self.body).map_err(invalid)
    }
}

fn invalid(error: impl std::fmt::Display) -> Error {
    Error::InvalidRequest(error.to_string())
}

/// `response` as `fetch.js` decodes it: the length of the JSON head as a
/// big-endian u32, the head (`status`, `headers`), then the body.
fn frame(response: &Response<Vec<u8>>) -> Result<Vec<u8>> {
    let head = Head {
        status: response.status().as_u16(),
        headers: response
            .headers()
            .iter()
            .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?)))
            .collect(),
    };
    let head = serde_json::to_vec(&head).map_err(invalid)?;
    let length = u32::try_from(head.len()).map_err(invalid)?;
    let body = response.body();
    let mut frame = Vec::with_capacity(4 + head.len() + body.len());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&head);
    frame.extend_from_slice(body);
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use axum::http::{StatusCode, header};
    use axum::routing::post;

    use super::*;
    use crate::dispatch::Dispatcher;
    use crate::proxy::Proxy;

    fn fetch_request(url: &str) -> FetchRequest {
        FetchRequest {
            method: "POST".to_owned(),
            url: url.to_owned(),
            headers: vec![(
                "content-type".to_owned(),
                "application/x-www-form-urlencoded".to_owned(),
            )],
            body: b"a=1".to_vec(),
        }
    }

    #[test]
    fn own_urls_become_origin_form_requests() {
        for url in [
            "leptos://localhost/api/greet?x=1",
            "http://leptos.localhost/api/greet?x=1",
        ] {
            let request = fetch_request(url).into_http().expect("own url");
            assert_eq!(request.method(), Method::POST);
            assert_eq!(request.uri(), "/api/greet?x=1");
            assert_eq!(
                request.headers()[header::CONTENT_TYPE],
                "application/x-www-form-urlencoded"
            );
            assert_eq!(request.body(), b"a=1");
        }
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
                matches!(fetch_request(url).into_http(), Err(Error::ForeignUrl(_))),
                "{url}"
            );
        }
    }

    #[test]
    fn frames_carry_head_and_body() {
        let mut response = Response::new(b"body".to_vec());
        *response.status_mut() = StatusCode::CREATED;
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));

        let frame = frame(&response).expect("frame");
        let length = u32::from_be_bytes(frame[..4].try_into().expect("4 bytes")) as usize;
        let head: serde_json::Value =
            serde_json::from_slice(&frame[4..4 + length]).expect("json head");
        assert_eq!(head["status"], 201);
        assert_eq!(head["headers"][0][0], "content-type");
        assert_eq!(head["headers"][0][1], "text/plain");
        assert_eq!(&frame[4 + length..], b"body");
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
        let request = fetch_request("leptos://localhost/echo")
            .into_http()
            .expect("own url");
        let response = dispatcher.dispatch(request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body(), b"a=1");
    }
}
