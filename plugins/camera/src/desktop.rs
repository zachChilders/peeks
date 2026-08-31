use serde::de::DeserializeOwned;
use tauri::{ipc::Channel, plugin::PluginApi, AppHandle, Runtime};

use crate::models::*;

pub fn init<R: Runtime, C: DeserializeOwned>(
    app: &AppHandle<R>,
    _api: PluginApi<R, C>,
) -> crate::Result<Camera<R>> {
    Ok(Camera(app.clone()))
}

/// No native camera/compass integration on desktop; every call reports unavailable / no-ops.
pub struct Camera<R: Runtime>(AppHandle<R>);

impl<R: Runtime> Camera<R> {
    pub fn start_camera(&self) -> crate::Result<()> {
        Err(crate::Error::Unsupported)
    }

    pub fn stop_camera(&self) -> crate::Result<()> {
        Ok(())
    }

    pub fn start_heading_updates<F: Fn(HeadingEvent) + Send + Sync + 'static>(
        &self,
        _callback: F,
    ) -> crate::Result<u32> {
        Ok(0)
    }

    pub(crate) fn start_heading_updates_inner(&self, _channel: Channel) -> crate::Result<()> {
        Ok(())
    }

    pub fn stop_heading_updates(&self) -> crate::Result<()> {
        Ok(())
    }

    pub fn start_motion_updates<F: Fn(MotionEvent) + Send + Sync + 'static>(
        &self,
        _callback: F,
    ) -> crate::Result<u32> {
        Ok(0)
    }

    pub(crate) fn start_motion_updates_inner(&self, _channel: Channel) -> crate::Result<()> {
        Ok(())
    }

    pub fn stop_motion_updates(&self) -> crate::Result<()> {
        Ok(())
    }

    pub fn start_intrinsics_updates<F: Fn(CameraIntrinsicsEvent) + Send + Sync + 'static>(
        &self,
        _callback: F,
    ) -> crate::Result<u32> {
        Ok(0)
    }

    pub(crate) fn start_intrinsics_updates_inner(&self, _channel: Channel) -> crate::Result<()> {
        Ok(())
    }

    pub fn stop_intrinsics_updates(&self) -> crate::Result<()> {
        Ok(())
    }

    pub fn start_frame_updates<F: Fn(FrameEvent) + Send + Sync + 'static>(
        &self,
        _callback: F,
    ) -> crate::Result<u32> {
        Ok(0)
    }

    pub(crate) fn start_frame_updates_inner(&self, _channel: Channel) -> crate::Result<()> {
        Ok(())
    }

    pub fn stop_frame_updates(&self) -> crate::Result<()> {
        Ok(())
    }

    pub fn capture_photo(&self) -> crate::Result<()> {
        Err(crate::Error::Unsupported)
    }
}
