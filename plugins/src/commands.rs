use tauri::{command, ipc::Channel, AppHandle, Runtime};

use crate::BarometerExt;
use crate::Result;

#[command]
pub(crate) async fn is_available<R: Runtime>(app: AppHandle<R>) -> Result<bool> {
    app.barometer().is_available()
}

#[command]
pub(crate) async fn start_updates<R: Runtime>(app: AppHandle<R>, channel: Channel) -> Result<()> {
    app.barometer().start_updates_inner(channel)
}

#[command]
pub(crate) async fn stop_updates<R: Runtime>(app: AppHandle<R>) -> Result<()> {
    app.barometer().stop_updates()
}
