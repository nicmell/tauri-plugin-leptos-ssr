//! Serves a cargo-leptos SSR app inside a Tauri app through the `leptos` URI
//! scheme: the app's router in release builds, the `cargo leptos watch` server
//! in dev builds.

use std::collections::HashSet;
use std::sync::Arc;

use leptos_config::{Env, LeptosOptions};
use tauri::plugin::{Builder, TauriPlugin};
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Manager, RunEvent, Runtime, WebviewUrl, WindowEvent};

mod assets;
mod commands;
mod dispatch;
mod error;
mod protocol;
mod proxy;
mod registry;
mod sockets;
mod streams;
#[cfg(test)]
mod testing;

pub use error::{BoxError, Error, Result};

use dispatch::Dispatcher;
use sockets::Sockets;
use streams::Streams;

const SCHEME: &str = "leptos";

/// The origin of the plugin's pages, as the webview reports it.
const ORIGIN: &str = if cfg!(any(windows, target_os = "android")) {
    "http://leptos.localhost"
} else {
    "leptos://localhost"
};

/// The plugin state, reachable through [`LeptosSsrExt`].
pub struct LeptosSsr {
    dispatcher: Dispatcher,
    streams: Streams,
    sockets: Sockets,
}

impl LeptosSsr {
    /// The webview URL of `path` on the plugin's scheme, `"/"` for the home page.
    pub fn webview_url(&self, path: &str) -> Result<WebviewUrl> {
        let base: url::Url = format!("{SCHEME}://localhost/").parse()?;
        Ok(WebviewUrl::CustomProtocol(base.join(path)?))
    }

    /// The origin of the plugin's pages: `leptos://localhost`, or
    /// `http://leptos.localhost` on Android and Windows.
    pub fn origin(&self) -> &'static str {
        ORIGIN
    }

    /// Drops what the page of `webview` held open: response bodies and
    /// websockets.
    fn close_webview(&self, webview: &str) {
        self.streams.close_webview(webview);
        self.sockets.close_webview(webview);
    }
}

/// Extensions to [`tauri::App`], [`tauri::AppHandle`] and [`tauri::Window`] to
/// access the plugin.
pub trait LeptosSsrExt<R: Runtime> {
    fn leptos_ssr(&self) -> &LeptosSsr;
}

impl<R: Runtime, T: Manager<R>> LeptosSsrExt<R> for T {
    fn leptos_ssr(&self) -> &LeptosSsr {
        self.state::<LeptosSsr>().inner()
    }
}

/// Initializes the plugin. `router` builds the app's router (pages and server
/// functions) from the app handle and the plugin's [`LeptosOptions`]. It runs
/// once, in release builds only: dev builds forward to `build.devUrl`. An
/// error from it fails the plugin's setup, so the app does not start.
pub fn init<R, F>(router: F) -> TauriPlugin<R>
where
    R: Runtime,
    F: FnOnce(&AppHandle<R>, LeptosOptions) -> std::result::Result<axum::Router, BoxError>
        + Send
        + 'static,
{
    Builder::new("leptos-ssr")
        .register_asynchronous_uri_scheme_protocol(SCHEME, protocol::handle)
        .invoke_handler(tauri::generate_handler![
            commands::fetch,
            commands::fetch_read_body,
            commands::fetch_cancel_body,
            commands::ws_open,
            commands::ws_read,
            commands::ws_send,
            commands::ws_close
        ])
        .js_init_script(fetch_script())
        .on_page_load(|webview, payload| {
            if payload.event() == PageLoadEvent::Started
                && let Some(state) = webview.try_state::<LeptosSsr>()
            {
                state.close_webview(webview.label());
            }
        })
        .on_event(|app, event| {
            if let RunEvent::WindowEvent {
                label,
                event: WindowEvent::Destroyed,
                ..
            } = event
                && let Some(state) = app.try_state::<LeptosSsr>()
            {
                state.close_webview(label);
            }
        })
        .setup(move |app, _api| {
            let dispatcher = if tauri::is_dev() {
                let upstream = app
                    .config()
                    .build
                    .dev_url
                    .clone()
                    .ok_or(Error::DevUrlUnset)?;
                log::info!("forwarding {SCHEME}://localhost to {upstream}");
                Dispatcher::Proxy(proxy::Proxy::new(&upstream)?)
            } else {
                Dispatcher::Router(release_router(app, router)?)
            };
            app.manage(LeptosSsr {
                dispatcher,
                streams: Streams::default(),
                sockets: Sockets::default(),
            });
            Ok(())
        })
        .build()
}

/// `fetch.js` for this platform's origin.
fn fetch_script() -> String {
    let origin = serde_json::to_string(ORIGIN).expect("a string serializes");
    include_str!("fetch.js").replace("__LEPTOS_SSR_ORIGIN__", &origin)
}

fn release_router<R, F>(app: &AppHandle<R>, router: F) -> Result<axum::Router>
where
    R: Runtime,
    F: FnOnce(&AppHandle<R>, LeptosOptions) -> std::result::Result<axum::Router, BoxError>,
{
    let output_name = option_env!("LEPTOS_OUTPUT_NAME").ok_or(Error::OutputNameUnset)?;
    let resolver = app.asset_resolver();
    // The bytes from `iter` are still compressed; content comes from `get`.
    let keys: HashSet<String> = resolver.iter().map(|(key, _)| key.into_owned()).collect();
    assets::check(&keys, output_name)?;
    log::info!(
        "serving {SCHEME}://localhost from the app, {} embedded files",
        keys.len()
    );

    let options = LeptosOptions::builder()
        .output_name(output_name)
        .env(Env::PROD)
        .build();
    // Leptos route generation spawns on the current tokio runtime.
    let runtime = tauri::async_runtime::handle();
    let _guard = runtime.inner().enter();
    let router = router(app, options).map_err(Error::Router)?;
    let resolver = Arc::new(resolver);
    Ok(assets::fallback(router, keys, move |path| {
        resolver
            .get(path.to_owned())
            .map(|asset| (asset.bytes, asset.mime_type))
    }))
}
