use tauri::{
  plugin::{Builder, TauriPlugin},
  Manager, Runtime,
};

pub use models::*;

#[cfg(desktop)]
mod desktop;
#[cfg(mobile)]
mod mobile;

mod commands;
mod error;
mod models;

pub use error::{Error, Result};

#[cfg(desktop)]
use desktop::LeptosSsr;
#[cfg(mobile)]
use mobile::LeptosSsr;

/// Extensions to [`tauri::App`], [`tauri::AppHandle`] and [`tauri::Window`] to access the leptos-ssr APIs.
pub trait LeptosSsrExt<R: Runtime> {
  fn leptos_ssr(&self) -> &LeptosSsr<R>;
}

impl<R: Runtime, T: Manager<R>> crate::LeptosSsrExt<R> for T {
  fn leptos_ssr(&self) -> &LeptosSsr<R> {
    self.state::<LeptosSsr<R>>().inner()
  }
}

/// Initializes the plugin.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
  Builder::new("leptos-ssr")
    .invoke_handler(tauri::generate_handler![commands::ping])
    .setup(|app, api| {
      #[cfg(mobile)]
      let leptos_ssr = mobile::init(app, api)?;
      #[cfg(desktop)]
      let leptos_ssr = desktop::init(app, api)?;
      app.manage(leptos_ssr);
      Ok(())
    })
    .build()
}
