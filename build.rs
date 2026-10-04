fn main() {
    tauri_plugin::Builder::new(&["fetch", "fetch_read_body", "fetch_cancel_body"]).build();
}
