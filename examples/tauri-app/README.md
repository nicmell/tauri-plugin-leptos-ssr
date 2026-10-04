# Tauri + Leptos SSR demo

A Leptos SSR app in the [start-axum-workspace](https://github.com/leptos-rs/start-axum-workspace) layout, served inside a Tauri app by `tauri-plugin-leptos-ssr`. It runs on macOS and Android.

```
app/        the Leptos app: pages, the `whoami` server function, `router()` (feature ssr)
frontend/   the wasm entry that hydrates the app (feature hydrate)
server/     the axum server that `cargo leptos watch` runs in dev, also a standalone web server
src-tauri/  the Tauri app: the plugin, the `greet` command, the main window
style/      the stylesheet that cargo-leptos compiles
public/     static files copied to the site root
```

The home page shows a self-check line after hydration. It calls the `whoami` server function and the `greet` Tauri command once. In dev the server function runs in the watch server (`server (macos)`). In a release build it runs inside the app (`tauri-app (macos)`, or `app_process64 (android)` on Android).

## Prerequisites

```bash
rustup target add wasm32-unknown-unknown
cargo install cargo-leptos tauri-cli --locked
```

For Android, install the Android SDK, NDK and JDK 17 or later, set `ANDROID_HOME` and `NDK_HOME`, and add the Rust targets:

```bash
rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android
```

Run all commands from this directory.

## macOS

```bash
cargo leptos build              # once: tauri-cli waits about 180 s for the dev server
cargo tauri dev                 # starts `cargo leptos watch` and the app
cargo tauri build               # bundles the app with the release site embedded
```

Edits in `app/`, `style/` and `public/` rebuild the watch server and reload the page. The app keeps running, because `.taurignore` keeps `tauri dev` from rebuilding it.

## Android

```bash
cargo tauri android init        # once: generates src-tauri/gen/android
cargo tauri android dev         # on a running emulator or a connected device
cargo tauri android build --apk
```

In dev the app on the device forwards every request to the watch server on your computer. On an emulator, tauri-cli runs `adb reverse` for port 3000, and `beforeDevCommand` runs it for port 3001, the live-reload port. With more than one device attached, the second `adb reverse` fails, so run it yourself with `-s <serial>`.

With tauri-cli 2.11.4, `cargo tauri android dev` stopped after Gradle built the APK and never installed it. If that happens, keep the command running and install the app yourself:

```bash
adb install -r src-tauri/gen/android/app/build/outputs/apk/arm64/debug/app-arm64-debug.apk
adb shell am start -n com.example.tauri_leptos_ssr/.MainActivity
```

On a physical device, tauri-cli replaces the dev URL with the LAN address of your computer. Make the watch server listen on it:

```bash
LEPTOS_SITE_ADDR=0.0.0.0:3000 cargo tauri android dev
```

## Checks

```bash
cargo fmt --check && leptosfmt --check app/src
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p frontend --features hydrate --target wasm32-unknown-unknown -- -D warnings
cargo test --workspace
```

`src-tauri/tests/config.rs` makes sure that `tauri.conf.json`, the cargo-leptos metadata and `.cargo/config.toml` agree.

## Dependencies

Every lockfile in this repo resolves only crates that are at least 7 days old. To refresh `Cargo.lock`, set the cutoff date and run:

```bash
RUSTC_BOOTSTRAP=1 cargo generate-lockfile -Zunstable-options --publish-time 2026-09-27T00:00:00Z
```
