# tauri-plugin-leptos-ssr

Tauri 2 plugin that serves a cargo-leptos SSR app through the `leptos` URI scheme. GET and HEAD go through the scheme. Requests with a body go through IPC: the `fetch` command and the injected `src/fetch.js`. Release builds dispatch to the app's axum router plus the embedded `frontendDist`. Dev builds forward to `build.devUrl`, the `cargo leptos watch` server. README.md has the full picture.

Scope is macOS and Android. Behavior must stay the same on both: a mechanism that cannot work on Android fails the same way on macOS (the scheme's 405 for methods with a body is the model).

## Layout

- `src/lib.rs`: `init(router)`, `LeptosSsr`, `webview_url`, setup (dev proxy or release router).
- `src/dispatch.rs`: the dispatcher behind both entry points: header hygiene, buffering, request log.
- `src/protocol.rs`: the scheme handler: GET/HEAD only, redirects as pages, CORS header.
- `src/proxy.rs`: dev forwarding with the hyper-util client, the retrying 502 page.
- `src/assets.rs`: the embedded-site fallback (exact keys) and the startup checks.
- `src/commands.rs` and `src/fetch.js`: the IPC path. They share the frame format, and tests on both sides pin it.
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
