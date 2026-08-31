use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadingReading {
    /// Heading relative to magnetic north, in degrees (0-360).
    pub magnetic_heading: f64,
    /// Heading relative to true north, in degrees (0-360). May equal magnetic heading
    /// if true-north correction is unavailable.
    pub true_heading: f64,
    /// Accuracy of the heading in degrees; negative means invalid.
    pub accuracy: f64,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HeadingEvent {
    Reading(HeadingReading),
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MotionReading {
    /// Camera tilt above horizontal, in degrees (0 = level, +90 = pointing at zenith).
    pub pitch: f64,
    /// Rotation about the camera's optical axis, in degrees (0 = top of phone points up).
    pub roll: f64,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MotionEvent {
    Reading(MotionReading),
    Error(String),
}

/// What the capture device actually reports about its optics, so the AR projection can
/// derive a real focal length instead of assuming an on-screen field of view. Emitted
/// once when the stream opens and again whenever zoom changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraIntrinsicsReading {
    /// Field of view across the capture buffer's long axis, in degrees, at zoom 1.0.
    pub fov_deg: f64,
    /// Current zoom, relative to the widest lens. 1.0 = unzoomed.
    pub zoom_factor: f64,
    /// Capture buffer dimensions in the sensor's native landscape orientation.
    pub buffer_long_px: f64,
    pub buffer_short_px: f64,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CameraIntrinsicsEvent {
    Reading(CameraIntrinsicsReading),
    Error(String),
}

/// A downsampled grayscale camera frame, for skyline fitting.
///
/// Delivered at a low rate (~2 Hz) and already reduced to ~160 px wide by the native side,
/// so this is a few tens of kilobytes rather than a full video frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameReading {
    pub width: usize,
    pub height: usize,
    /// Base64 `width * height` grayscale bytes, row-major.
    pub gray: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FrameEvent {
    Reading(FrameReading),
    Error(String),
}
