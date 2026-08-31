//! The bundled named-peak dataset, loaded once from app resources.
//!
//! Replaces the Overpass query that used to run on every launch. Format, tile indexing,
//! and the radius query live in `peakcore::peakfile` (shared with the peaklab subcommand
//! that generates the file, so writer and reader can't drift); this module adds resource
//! resolution and the Tauri-managed handle.
//!
//! Unlike `DemCache` there is nothing to fetch or evict — the file ships with the app, so
//! this is a read-only load-once. That is the entire point: peaks now resolve with no
//! network at all, which is what makes the app work where there is no signal.

use peakcore::peakfile::PeakFile;
use tauri::Manager;
use tokio::sync::Mutex;

/// Name the file is bundled under, matching the `bundle.resources` mapping in
/// `tauri.conf.json`.
const RESOURCE_NAME: &str = "peaks.mvpk";

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not resolve the bundled peak dataset: {0}")]
    Tauri(#[from] tauri::Error),
    #[error("could not read the bundled peak dataset: {0}")]
    Io(#[from] std::io::Error),
    #[error("bundled peak dataset is malformed: {0}")]
    Format(#[from] peakcore::peakfile::FormatError),
}

/// Tauri-managed state: the bundled dataset, parsed on first use and resident after.
///
/// A `tokio::sync::Mutex` for the same reason as [`crate::dem::DemCache`] — the load
/// holds the guard across an `.await` while reading the file.
#[derive(Default)]
pub struct PeakStore(Mutex<Option<PeakFile>>);

impl PeakStore {
    /// Every named peak within `radius_m` of `(lat, lon)`, loading the dataset if this is
    /// the first call.
    ///
    /// The ~20 MB file stays resident once parsed; only the tiles a query touches are
    /// decoded into records, so this does not build 690k `String`s to answer one query.
    pub async fn peaks_in_radius<R: tauri::Runtime>(
        &self,
        app: &tauri::AppHandle<R>,
        lat: f64,
        lon: f64,
        radius_m: f64,
    ) -> Result<Vec<peakcore::peakfile::Record>> {
        let mut guard = self.0.lock().await;

        if guard.is_none() {
            let path = app
                .path()
                .resolve(RESOURCE_NAME, tauri::path::BaseDirectory::Resource)?;
            let bytes = tokio::fs::read(&path).await?;
            *guard = Some(PeakFile::parse(bytes)?);
        }

        let file = guard.as_ref().expect("just loaded above");
        Ok(file.peaks_in_radius(lat, lon, radius_m)?)
    }
}
