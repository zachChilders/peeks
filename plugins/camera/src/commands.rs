use tauri::{command, ipc::Channel, AppHandle, Runtime};

use crate::CameraExt;
use crate::Result;

#[command]
pub(crate) async fn start_camera<R: Runtime>(app: AppHandle<R>) -> Result<()> {
    app.camera().start_camera()
}

#[command]
pub(crate) async fn stop_camera<R: Runtime>(app: AppHandle<R>) -> Result<()> {
    app.camera().stop_camera()
}

#[command]
pub(crate) async fn start_heading_updates<R: Runtime>(
    app: AppHandle<R>,
    channel: Channel,
) -> Result<()> {
    app.camera().start_heading_updates_inner(channel)
}

#[command]
pub(crate) async fn stop_heading_updates<R: Runtime>(app: AppHandle<R>) -> Result<()> {
    app.camera().stop_heading_updates()
}

#[command]
pub(crate) async fn start_motion_updates<R: Runtime>(
    app: AppHandle<R>,
    channel: Channel,
) -> Result<()> {
    app.camera().start_motion_updates_inner(channel)
}

#[command]
pub(crate) async fn stop_motion_updates<R: Runtime>(app: AppHandle<R>) -> Result<()> {
    app.camera().stop_motion_updates()
}
