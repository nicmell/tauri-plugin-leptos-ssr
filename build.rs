fn main() {
    tauri_plugin::Builder::new(&[
        "fetch",
        "fetch_read_body",
        "fetch_cancel_body",
        "ws_open",
        "ws_read",
        "ws_send",
        "ws_close",
    ])
    .build();
}
