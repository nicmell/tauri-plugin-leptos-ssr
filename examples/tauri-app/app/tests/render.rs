use axum::body::Body;
use axum::http::{Request, StatusCode};
use leptos::prelude::LeptosOptions;
use tower::ServiceExt;

#[tokio::test]
async fn home_renders_on_the_server() {
    let options = LeptosOptions::builder()
        .output_name(env!("LEPTOS_OUTPUT_NAME"))
        .build();
    let request = Request::get("/")
        .body(Body::empty())
        .expect("valid request");
    let Ok(response) = app::router(options).oneshot(request).await;
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let html = String::from_utf8(body.to_vec()).expect("utf-8 html");
    assert!(html.contains("<h1>Tauri + Leptos SSR</h1>"), "{html}");
    assert!(html.contains("/pkg/tauri-app.wasm"), "{html}");
    assert!(!html.contains("_bg.wasm"), "{html}");
}
