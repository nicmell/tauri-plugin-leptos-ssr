# tauri-plugin-leptos-ssr

A Tauri 2 plugin that serves a [cargo-leptos](https://github.com/leptos-rs/cargo-leptos) SSR app inside the Tauri app. The app opens no TCP port. Pages, server functions and static files take the same path on macOS and Android, in dev and in release builds.

Tested on macOS (dev and release bundle) and on an Android 16 emulator (dev, debug and release APKs). Other platforms are untested.

## How it works

The plugin registers the URI scheme `leptos`. The window loads `leptos://localhost/` (the webview shows `http://leptos.localhost/` on Android). Every request goes to one dispatcher:

- In release builds, the dispatcher is your app's axum router, rendered in-process. The files that `cargo leptos build` writes to the site root are embedded in the binary through `build.frontendDist` and serve as the fallback.
- In dev builds, the dispatcher forwards each request to `build.devUrl`, the `cargo leptos watch` server.

The scheme answers GET and HEAD. Custom-protocol requests reach the app without a body on Android, so the scheme refuses every other method with a 405, on every platform.

Requests with a body take the IPC path. The plugin injects a script that wraps `window.fetch` on the plugin origin. A same-origin request whose method is not GET or HEAD goes through the plugin's `fetch` command, which hands it to the same dispatcher. Leptos server functions call `fetch`, so they work without changes.

Responses on the IPC path stream. The `fetch` command answers with the head and the bytes already available. The script then reads the rest into a `ReadableStream`, one `fetch_read_body` call per chunk, so streaming server functions arrive chunk by chunk. The scheme cannot stream, because Tauri's scheme responder takes a complete body. For that reason, a same-origin GET that accepts `text/event-stream` also takes the IPC path. The script also replaces `EventSource` on the plugin origin with one built on that fetch.

When a page loads or its window closes, the plugin drops every open response body of that webview. In dev, that closes the connection to the watch server. In release builds, it stops the app's stream.

Every request the plugin dispatches carries a `leptos-ssr-origin` header with the page origin, in dev and in release. The plugin replaces any value that the page sent. With it, the app's server tells its webview from a browser, for example to serve the webview another script.

## Setup

These steps follow the demo in [`examples/tauri-app`](examples/tauri-app), which uses the [start-axum-workspace](https://github.com/leptos-rs/start-axum-workspace) layout.

1. Give your Leptos app crate a function that returns its router, behind the `ssr` feature. Do not set a fallback on it, because the plugin sets its own.

   ```rust
   pub fn router(options: LeptosOptions) -> axum::Router {
       let routes = generate_route_list(App);
       axum::Router::new()
           .leptos_routes(&options, routes, {
               let options = options.clone();
               move || shell(options.clone())
           })
           .with_state(options)
   }
   ```

2. Add the plugin and your app crate with `ssr` to the Tauri crate. Register the plugin and open the window on the plugin's scheme:

   ```rust
   use tauri_plugin_leptos_ssr::LeptosSsrExt;

   tauri::Builder::default()
       .plugin(tauri_plugin_leptos_ssr::init(app::router))
       .setup(|app| {
           let url = app.leptos_ssr().webview_url("/")?;
           tauri::WebviewWindowBuilder::new(app, "main", url).build()?;
           Ok(())
       })
   ```

3. Set the `build` section of `tauri.conf.json`, and set `app.windows` to `[]`:

   ```json
   "build": {
     "beforeDevCommand": { "script": "cargo leptos watch", "cwd": "..", "wait": false },
     "beforeBuildCommand": { "script": "cargo leptos build --release --frontend-only", "cwd": ".." },
     "devUrl": "http://127.0.0.1:3000",
     "frontendDist": "../target/site"
   }
   ```

   `devUrl` must be the cargo-leptos `site-addr`, and `frontendDist` must be its `site-root`. For live reload on an Android emulator, the demo's `beforeDevCommand` also runs `adb reverse tcp:3001 tcp:3001`.

4. Set the cargo-leptos `name` at compile time in `.cargo/config.toml`:

   ```toml
   [env]
   LEPTOS_OUTPUT_NAME = "your-app"
   ```

5. Add `leptos-ssr:default` to the capability of the window. It allows the `fetch` command.

6. Add a `.taurignore` next to the cargo-leptos workspace manifest that lists your Leptos crates, for example `/app`.

A server outside the plugin, for example a websocket server in the app, sees the pages' `Origin` header. `app.leptos_ssr().origin()` returns that origin for its checks.

## Build requirements

The Tauri build compiles the SSR side with plain cargo, not with cargo-leptos. These requirements follow from that:

- Build the site with `--release`. cargo-leptos dev builds compile leptos with `--cfg erase_components` and, under `watch`, with hot-reload markers. The SSR code in the Tauri binary has neither, so only a release site matches it.
- Set `LEPTOS_OUTPUT_NAME`. Leptos reads it at compile time to name the wasm bundle. Without it, pages ask for `/pkg/<name>_bg.wasm`, which cargo-leptos does not write. When it is missing, release builds of the plugin refuse to start.
- Point `frontendDist` at the site root. When `/pkg/<name>.wasm` or `/pkg/<name>.js` is not embedded, release builds refuse to start.
- If you set `server-fn-prefix`, `disable-server-fn-hash` or `server-fn-mod-path` in the cargo-leptos metadata, set `SERVER_FN_PREFIX`, `DISABLE_SERVER_FN_HASH` or `SERVER_FN_MOD_PATH` in `.cargo/config.toml` too. Leptos reads them at compile time.
- Keep `hash-files` off. The plugin serves the bundle under its plain name.
- The `.taurignore` matters in dev. `tauri dev` watches the path dependencies of the Tauri crate. Without it, every UI edit restarts the app, although in dev the app only forwards to the watch server.

In dev builds, server functions run in the `cargo leptos watch` process. That process has no Tauri `AppHandle`. Call Tauri commands from the browser for native work, as the demo does with `greet`.

## Limits

These hold on every platform:

- Pages and static files are buffered. SSR streaming arrives in one piece.
- A GET `fetch` is buffered unless it accepts `text/event-stream`.
- Request bodies are buffered. On Android, Tauri sends them as JSON number arrays of about 4 times their size, so keep uploads to a few MB.
- No websockets.
- No cookies. Neither scheme responses nor IPC responses reach the webview cookie store.
- A native `<form method="post">` submitted before hydration gets the 405.
- `XMLHttpRequest` requests with a body are not rerouted. Leptos only uses `fetch`.
- Redirects through the scheme become pages that load the new location, because wry on Android drops 3xx responses.
- `app.security.csp` and `app.security.headers` apply to `tauri://` responses only, not to the plugin's pages.
- `useHttpsScheme` is not supported.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
node --test 'tests/*.test.mjs'   # src/fetch.js against a stubbed page, Node 22 or later
```

The demo has its own commands in [`examples/tauri-app/README.md`](examples/tauri-app/README.md).

## License

MIT-0, see [LICENSE](LICENSE).
