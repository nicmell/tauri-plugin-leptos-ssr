use leptos::prelude::get_configuration;
use tower_http::services::ServeDir;

#[tokio::main]
async fn main() {
    let options = get_configuration(None)
        .expect("cargo-leptos configuration")
        .leptos_options;
    let addr = options.site_addr;
    let site_root = options.site_root.to_string();
    let app = app::router(options).fallback_service(ServeDir::new(site_root));

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind the site address");
    println!("listening on http://{addr}");
    axum::serve(listener, app).await.expect("serve the site");
}
