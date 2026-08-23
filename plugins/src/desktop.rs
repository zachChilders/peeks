use serde::de::DeserializeOwned;
use tauri::{ipc::Channel, plugin::PluginApi, AppHandle, Runtime};

use crate::models::*;

pub fn init<R: Runtime, C: DeserializeOwned>(
    app: &AppHandle<R>,
    _api: PluginApi<R, C>,
) -> crate::Result<Barometer<R>> {
    Ok(Barometer(app.clone()))
}

/// Desktop has no barometer; every call reports unavailable / no-ops.
pub struct Barometer<R: Runtime>(AppHandle<R>);

impl<R: Runtime> Barometer<R> {
    pub fn is_available(&self) -> crate::Result<bool> {
        Ok(false)
    }

    pub fn start_updates<F: Fn(AltitudeEvent) + Send + Sync + 'static>(
        &self,
        _callback: F,
    ) -> crate::Result<u32> {
        Ok(0)
    }

    pub(crate) fn start_updates_inner(&self, _channel: Channel) -> crate::Result<()> {
        Ok(())
    }

    pub fn stop_updates(&self) -> crate::Result<()> {
        Ok(())
    }
}
