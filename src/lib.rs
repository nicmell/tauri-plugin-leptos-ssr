use tauri::{
    Runtime,
    plugin::{Builder, TauriPlugin},
};

mod error;

pub use error::{Error, Result};

/// Initializes the plugin.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("leptos-ssr").build()
}
