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
use desktop::Barometer;
#[cfg(mobile)]
use mobile::Barometer;

/// Extensions to [`tauri::App`], [`tauri::AppHandle`] and [`tauri::Window`] to access the barometer APIs.
pub trait BarometerExt<R: Runtime> {
  fn barometer(&self) -> &Barometer<R>;
}

impl<R: Runtime, T: Manager<R>> crate::BarometerExt<R> for T {
  fn barometer(&self) -> &Barometer<R> {
    self.state::<Barometer<R>>().inner()
  }
}

/// Initializes the plugin.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
  Builder::new("barometer")
    .invoke_handler(tauri::generate_handler![
      commands::is_available,
      commands::start_updates,
      commands::stop_updates
    ])
    .setup(|app, api| {
      #[cfg(mobile)]
      let barometer = mobile::init(app, api)?;
      #[cfg(desktop)]
      let barometer = desktop::init(app, api)?;
      app.manage(barometer);
      Ok(())
    })
    .build()
}
