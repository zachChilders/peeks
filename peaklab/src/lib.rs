//! Desktop harness for the AR peak-identification pipeline.
//!
//! Milestones, in dependency order:
//! - [`dem`] — elevation sampling (M0)
//! - [`geo`] — observer pose and look angles (M1), re-exported from `peakcore`
//! - [`peaks`] — named peaks from OpenStreetMap (M2)
//! - [`visibility`] — terrain occlusion via raycasting (M3)
//! - [`projection`] — camera pose, projection, label layout (M4), re-exported from
//!   `peakcore`
//! - [`render`] — drawing labels onto an image

pub mod dem;
pub mod peaks;
pub mod render;
pub mod visibility;

pub use peakcore::{geo, projection};

/// Eye height above ground for a standing observer, in metres.
pub const EYE_HEIGHT_M: f64 = 1.6;

/// Default directory holding cached DEM tiles and Overpass responses.
pub fn data_dir() -> std::path::PathBuf {
    std::env::var_os("PEAKLAB_DATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data"))
}
