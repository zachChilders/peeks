use serde::de::DeserializeOwned;
use tauri::{
    ipc::{Channel, InvokeResponseBody},
    plugin::{PluginApi, PluginHandle},
    AppHandle, Runtime,
};

use crate::models::*;

#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_barometer);

// initializes the Swift plugin class
pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> crate::Result<Barometer<R>> {
    #[cfg(target_os = "ios")]
    let handle = api.register_ios_plugin(init_plugin_barometer)?;
    Ok(Barometer(handle))
}

/// Access to the barometer APIs.
pub struct Barometer<R: Runtime>(PluginHandle<R>);

impl<R: Runtime> Barometer<R> {
    pub fn is_available(&self) -> crate::Result<bool> {
        #[derive(serde::Deserialize)]
        struct Response {
            available: bool,
        }
        let response: Response = self.0.run_mobile_plugin("isAvailable", ())?;
        Ok(response.available)
    }

    /// Start streaming relative altitude updates. Returns a channel id to use in `stop_updates`.
    pub fn start_updates<F: Fn(AltitudeEvent) + Send + Sync + 'static>(
        &self,
        callback: F,
    ) -> crate::Result<u32> {
        let channel = Channel::new(move |event| {
            let payload = match event {
                InvokeResponseBody::Json(payload) => serde_json::from_str::<AltitudeEvent>(&payload)
                    .unwrap_or_else(|error| {
                        AltitudeEvent::Error(format!(
                            "Couldn't deserialize altitude event payload: `{error}`"
                        ))
                    }),
                _ => AltitudeEvent::Error("Unexpected altitude event payload.".to_string()),
            };

            callback(payload);

            Ok(())
        });
        let id = channel.id();

        self.start_updates_inner(channel)?;

        Ok(id)
    }

    pub(crate) fn start_updates_inner(&self, channel: Channel) -> crate::Result<()> {
        self.0
            .run_mobile_plugin("startUpdates", StartUpdatesPayload { channel })
            .map_err(Into::into)
    }

    pub fn stop_updates(&self) -> crate::Result<()> {
        self.0
            .run_mobile_plugin("stopUpdates", ())
            .map_err(Into::into)
    }
}

#[derive(serde::Serialize)]
struct StartUpdatesPayload {
    channel: Channel,
}
