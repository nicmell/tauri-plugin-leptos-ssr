use std::collections::HashSet;
use std::sync::Arc;

use axum::http::{Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use percent_encoding::percent_decode_str;

use crate::{Error, Result};

/// Fails unless `keys` holds the bundle cargo-leptos builds for `output_name`.
pub(crate) fn check(keys: &HashSet<String>, output_name: &str) -> Result<()> {
    for extension in ["js", "wasm"] {
        let key = format!("/pkg/{output_name}.{extension}");
        if !keys.contains(&key) {
            return Err(Error::MissingAsset(key));
        }
    }
    Ok(())
}

/// `router` with a fallback serving the embedded site: `keys` are the asset
/// paths, `load` reads one asset by its raw request path.
pub(crate) fn fallback<L>(router: axum::Router, keys: HashSet<String>, load: L) -> axum::Router
where
    L: Fn(&str) -> Option<(Vec<u8>, String)> + Clone + Send + Sync + 'static,
{
    let keys = Arc::new(keys);
    router.fallback(move |method: Method, uri: Uri| {
        let keys = keys.clone();
        let load = load.clone();
        async move { serve(&keys, &load, &method, &uri) }
    })
}

fn serve<L>(keys: &HashSet<String>, load: &L, method: &Method, uri: &Uri) -> Response
where
    L: Fn(&str) -> Option<(Vec<u8>, String)>,
{
    if !matches!(*method, Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let path = uri.path();
    // Tauri's lookup decodes the path itself and falls back to index.html, so
    // only exact keys reach it.
    let known = percent_decode_str(path)
        .decode_utf8()
        .is_ok_and(|decoded| keys.contains(decoded.as_ref()));
    match known.then(|| load(path)).flatten() {
        Some((bytes, mime_type)) => {
            // Tauri has no mime type for .wasm; wasm-bindgen streams the module
            // only when it is served as application/wasm.
            let mime_type = if path.ends_with(".wasm") {
                "application/wasm".to_owned()
            } else {
                mime_type
            };
            ([(header::CONTENT_TYPE, mime_type)], bytes).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;

    fn keys() -> HashSet<String> {
        ["/pkg/app.js", "/pkg/app.wasm", "/a b.txt"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    // The loader answers every path, the way Tauri's index.html fallback does.
    fn site() -> axum::Router {
        fallback(
            axum::Router::new().route("/", get(|| async { "page" })),
            keys(),
            |path: &str| Some((path.as_bytes().to_vec(), "text/javascript".to_owned())),
        )
    }

    async fn call(method: Method, uri: &str) -> (StatusCode, Option<String>, String) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .expect("valid request");
        let Ok(response) = site().oneshot(request).await;
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        (
            status,
            content_type,
            String::from_utf8_lossy(&body).into_owned(),
        )
    }

    #[tokio::test]
    async fn routes_win_over_assets() {
        let (status, _, body) = call(Method::GET, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "page");
    }

    #[tokio::test]
    async fn known_assets_are_served() {
        let (status, content_type, body) = call(Method::GET, "/pkg/app.js").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type.as_deref(), Some("text/javascript"));
        assert_eq!(body, "/pkg/app.js");
    }

    #[tokio::test]
    async fn wasm_is_served_as_wasm() {
        let (_, content_type, _) = call(Method::GET, "/pkg/app.wasm").await;
        assert_eq!(content_type.as_deref(), Some("application/wasm"));
    }

    #[tokio::test]
    async fn unknown_paths_are_not_found() {
        for path in ["/index.html", "/missing", "/pkg/../pkg/app.js"] {
            let (status, _, _) = call(Method::GET, path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[tokio::test]
    async fn encoded_paths_match_decoded_keys() {
        let (status, _, body) = call(Method::GET, "/a%20b.txt").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "/a%20b.txt");
    }

    #[tokio::test]
    async fn head_has_no_body() {
        let (status, _, body) = call(Method::HEAD, "/pkg/app.js").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn assets_refuse_other_methods() {
        let (status, _, _) = call(Method::POST, "/pkg/app.js").await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn check_requires_the_bundle() {
        assert!(check(&keys(), "app").is_ok());
        let mut keys = keys();
        keys.remove("/pkg/app.wasm");
        assert!(matches!(
            check(&keys, "app"),
            Err(Error::MissingAsset(key)) if key == "/pkg/app.wasm"
        ));
    }
}
