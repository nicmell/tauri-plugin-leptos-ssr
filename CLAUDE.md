# tauri-plugin-leptos-ssr

Tauri 2 plugin that serves a cargo-leptos SSR app through the `leptos` URI scheme. GET and HEAD go through the scheme, buffered. Requests with a body, and GETs that accept `text/event-stream`, go through IPC with streamed responses: the `fetch`, `fetch_read_body` and `fetch_cancel_body` commands and the injected `src/fetch.js`, which also replaces `EventSource` and `WebSocket` on the plugin origin. Websockets use the `ws_open`, `ws_read`, `ws_send` and `ws_close` commands. Release builds dispatch to the app's axum router plus the embedded `frontendDist`. Dev builds forward to `build.devUrl`, the `cargo leptos watch` server. README.md has the full picture.

Scope is macOS and Android. Behavior must stay the same on both: a mechanism that cannot work on Android fails the same way on macOS (the scheme's 405 for methods with a body is the model).

## Layout

- `src/lib.rs`: `init(router)`, `LeptosSsr`, `webview_url`, setup (dev proxy or release router). The router closure gets the `AppHandle` and can fail.
- `src/dispatch.rs`: the dispatcher behind both entry points: header hygiene, request log, the 500 for a panic in the app. It returns unread bodies.
- `src/protocol.rs`: the scheme handler: GET/HEAD only, buffered within 20 s, redirects as pages, CORS header.
- `src/proxy.rs`: dev forwarding with the hyper-util client, the retrying 502 page.
- `src/assets.rs`: the embedded-site fallback (exact keys) and the startup checks.
- `src/registry.rs`: what pages hold open, per webview: ids, and the page generations that drop a reloaded page's leftovers.
- `src/calls.rs`: the `fetch` and `ws_open` calls that each webview started, by id. On macOS, Tauri resends the pending calls of a page that loads another URL, and each id runs once.
- `src/streams.rs`: the open response bodies per webview: chunked reads with a 20 s idle answer, cancel, cleanup on page load and window close.
- `src/sockets.rs`: the open websockets per webview. A reader and a writer task each, the 1 MiB read-ahead, the record format, and close with 1001 on page load and window close. Release builds connect through `Dispatcher::connect`, an in-memory hyper connection to the router.
- `src/commands.rs` and `src/fetch.js`: the IPC path. They share the response frame, the chunk flags and the socket records. `tests/wire.json` pins them for the Rust tests and, through `tests/wire.mjs`, for the node tests. In its byte lists, numbers are bytes and strings are UTF-8 text.
- `examples/tauri-app`: the demo, its own Cargo workspace with a committed lockfile. Its README has its commands.

## Commands

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
node --test 'tests/*.test.mjs'
```

## Conventions

- 7-day package floor. Create lockfiles with
  `RUSTC_BOOTSTRAP=1 cargo generate-lockfile -Zunstable-options --publish-time <today minus 7 days>T00:00:00Z`
  (`cargo update` has no such flag). The root `Cargo.lock` stays untracked. The example's is committed.
- Rustfmt defaults, `leptosfmt` for the example's views.
- Comments state only facts the code cannot (traps, platform quirks). Rationale goes to the READMEs.
- New features go on their own branch and merge through a GitHub PR. One step per commit.
