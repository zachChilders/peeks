use serde::de::DeserializeOwned;
use tauri::{
    ipc::{Channel, InvokeResponseBody},
    plugin::{PluginApi, PluginHandle},
    AppHandle, Runtime,
};

use crate::models::*;

#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_camera);

// initializes the Swift plugin class
pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> crate::Result<Camera<R>> {
    #[cfg(target_os = "ios")]
    let handle = api.register_ios_plugin(init_plugin_camera)?;
    Ok(Camera(handle))
}

/// Access to the camera + compass APIs.
pub struct Camera<R: Runtime>(PluginHandle<R>);

impl<R: Runtime> Camera<R> {
    /// Start the native camera preview, layered behind the (now-transparent) webview.
    pub fn start_camera(&self) -> crate::Result<()> {
        self.0
            .run_mobile_plugin("startCamera", ())
            .map_err(Into::into)
    }

    pub fn stop_camera(&self) -> crate::Result<()> {
        self.0
            .run_mobile_plugin("stopCamera", ())
            .map_err(Into::into)
    }

    /// Start streaming compass heading updates. Returns a channel id (unused for stopping;
    /// only one heading stream is supported at a time — call `stop_heading_updates` to end it).
    pub fn start_heading_updates<F: Fn(HeadingEvent) + Send + Sync + 'static>(
        &self,
        callback: F,
    ) -> crate::Result<u32> {
        let channel = Channel::new(move |event| {
            let payload = match event {
                InvokeResponseBody::Json(payload) => serde_json::from_str::<HeadingEvent>(&payload)
                    .unwrap_or_else(|error| {
                        HeadingEvent::Error(format!(
                            "Couldn't deserialize heading event payload: `{error}`"
                        ))
                    }),
                _ => HeadingEvent::Error("Unexpected heading event payload.".to_string()),
            };

            callback(payload);

            Ok(())
        });
        let id = channel.id();

        self.start_heading_updates_inner(channel)?;

        Ok(id)
    }

    pub(crate) fn start_heading_updates_inner(&self, channel: Channel) -> crate::Result<()> {
        self.0
            .run_mobile_plugin("startHeadingUpdates", StartHeadingPayload { channel })
            .map_err(Into::into)
    }

    pub fn stop_heading_updates(&self) -> crate::Result<()> {
        self.0
            .run_mobile_plugin("stopHeadingUpdates", ())
            .map_err(Into::into)
    }
}

#[derive(serde::Serialize)]
struct StartHeadingPayload {
    channel: Channel,
}
