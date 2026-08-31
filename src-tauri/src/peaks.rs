//! Named peaks (from the bundled dataset), snapped to and elevated from the same local
//! Copernicus GLO-30 DEM used for terrain occlusion — see [`crate::dem`] and
//! [`peakcore::dem`].
//!
//! Peaks previously came from a live Overpass query on every launch. That was a hard
//! network dependency in an app used where there is no signal, and `overpass-api.de`
//! round-robins across backends of which one was reliably returning 504s. Since the only
//! values that survived the fetch were the name and the OSM id — every coordinate and
//! elevation below is overwritten by the DEM snap — the whole thing is now a ~20 MB file
//! shipped with the app; see [`crate::peakstore`].
//!
//! Previously used Open-Elevation for both peak and observer elevation: a different
//! dataset than the DEM the occlusion raycast samples, with no guarantee the two would
//! agree. That mismatch could make a peak's own elevation angle read as above or below
//! the terrain angle right at the margin independent of whether it was actually visible.
//! Mirrors `peaklab::peaks::resolve`, which does the same DEM snap/elevation lookup on
//! desktop.
//!
//! Consolidates what `src/lib/peaks.ts` did in TypeScript, plus the `fetchElevation`
//! helper that `MapView.tsx` and `CameraView.tsx` each duplicated verbatim for their own
//! single-point lookups — both now call the same [`get_elevation`] command.

use peakcore::geo::Geodetic;
use peakcore::visibility::{self, VisibilityConfig};
use serde::{Deserialize, Serialize};
use specta::Type;
use specta_typescript::Number;

/// DEM postings (~30 m) to search around each OSM peak node for the true local summit.
/// Matches peaklab's own default: measured against 344 peaks' tagged elevations near
/// Rainier, wider windows mostly climb onto neighbouring terrain rather than fixing
/// anything — the count of peaks reading >50 m above their tagged elevation roughly
/// triples from window=0 to window=240 m (see `peaklab/src/peaks.rs`).
const SNAP_HALF_WINDOW: i64 = 1;

/// Extra margin, in metres, when loading the DEM for a peak fetch beyond the query
/// radius itself — guarantees the snap window has coverage even for a peak right at the
/// edge of that radius.
const SNAP_DEM_MARGIN_M: f64 = 2_000.0;

/// Azimuth spacing for the debug DEM-horizon skyline. Matches `visibility::check`'s own
/// along-path step for the ray-march distance step; this is the angular step of a full
/// 360° sweep around the observer instead.
const HORIZON_AZIMUTH_STEP_DEG: f64 = 2.0;
const HORIZON_RAY_STEP_M: f64 = 60.0;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    PeakStore(#[from] crate::peakstore::Error),
    #[error(transparent)]
    Dem(#[from] crate::dem::Error),
    #[error("no DEM coverage at {lat}, {lon}")]
    NoDemCoverage { lat: f64, lon: f64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct Peak {
    pub name: String,
    // See scene.rs's PeakWithMetrics::osm_id for why this needs the Number override.
    #[specta(type = Number)]
    pub osm_id: i64,
    /// Snapped to the highest nearby DEM posting.
    pub lat: f64,
    pub lon: f64,
    /// DEM surface elevation at the snapped position, metres.
    pub elev: f64,
}

/// Resolve dataset records against the DEM: snap each one to the highest posting within
/// [`SNAP_HALF_WINDOW`] and take its elevation from there.
///
/// The dataset's own coordinates are only a seed for this search — every value the app
/// displays comes out of the DEM, which is what keeps peak elevations consistent with the
/// terrain the occlusion raycast samples. A peak outside DEM coverage is dropped.
async fn snap_to_dem<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    dem_cache: &crate::dem::DemCache,
    raw: Vec<peakcore::peakfile::Record>,
    lat: f64,
    lon: f64,
    radius_m: f64,
) -> Result<Vec<Peak>> {
    dem_cache
        .load_region(app, lat, lon, radius_m + SNAP_DEM_MARGIN_M)
        .await?;

    Ok(dem_cache
        .with_dem(|dem| {
            raw.into_iter()
                .filter_map(|r| {
                    let (lat, lon, elev) = dem.local_max(r.lat, r.lon, SNAP_HALF_WINDOW)?;
                    Some(Peak {
                        name: r.name,
                        osm_id: r.osm_id,
                        lat,
                        lon,
                        elev,
                    })
                })
                .collect()
        })
        .await)
}

async fn fetch_peaks_impl<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    dem_cache: &crate::dem::DemCache,
    peak_store: &crate::peakstore::PeakStore,
    lat: f64,
    lon: f64,
    radius_m: f64,
) -> Result<Vec<Peak>> {
    let raw = peak_store.peaks_in_radius(app, lat, lon, radius_m).await?;
    snap_to_dem(app, dem_cache, raw, lat, lon, radius_m).await
}

async fn get_elevation_impl<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    dem_cache: &crate::dem::DemCache,
    lat: f64,
    lon: f64,
) -> Result<f64> {
    dem_cache.load_region(app, lat, lon, 0.0).await?;
    dem_cache
        .with_dem(|dem| dem.elevation_at(lat, lon))
        .await
        .ok_or(Error::NoDemCoverage { lat, lon })
}

/// Drops peaks whose line of sight from `observer` is blocked by intervening terrain.
///
/// Raycasts every ~60 m along the path to each peak (`peakcore::visibility::check`)
/// against the same local Copernicus GLO-30 DEM `fetch_peaks` snapped peaks against,
/// instead of a handful of points sampled over the network: real ridgelines only a
/// posting or two wide were falling entirely between the old sparse network samples and
/// reading as clear sightlines when they were not. The region covering `radius_m` around
/// `observer` is downloaded once and cached in `dem_cache` (in memory for the session, on
/// disk across launches), so the dense raycast itself costs no network round trips.
async fn filter_visible_peaks_impl<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    dem_cache: &crate::dem::DemCache,
    observer: Geodetic,
    peaks: Vec<Peak>,
    radius_m: f64,
) -> Result<Vec<Peak>> {
    dem_cache.load_region(app, observer.lat, observer.lon, radius_m).await?;

    Ok(dem_cache
        .with_dem(|dem| {
            peaks
                .into_iter()
                .filter(|peak| {
                    let target = Geodetic::new(peak.lat, peak.lon, peak.elev);
                    visibility::check(dem, observer, target, VisibilityConfig::default()).is_visible()
                })
                .collect()
        })
        .await)
}

/// The debug DEM-horizon skyline: apparent terrain elevation angle in every direction
/// around `observer` out to `max_range_m`, for a debug overlay comparing what the
/// occlusion check "sees" against what the camera actually sees. Not used by
/// `filter_visible_peaks` itself — see `peakcore::visibility::horizon_at_azimuth`.
async fn compute_horizon_impl<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    dem_cache: &crate::dem::DemCache,
    observer: Geodetic,
    max_range_m: f64,
) -> Result<Vec<(f64, f64)>> {
    dem_cache.load_region(app, observer.lat, observer.lon, max_range_m).await?;

    Ok(dem_cache
        .with_dem(|dem| {
            visibility::sweep_horizon(
                dem,
                observer,
                max_range_m,
                HORIZON_AZIMUTH_STEP_DEG,
                HORIZON_RAY_STEP_M,
            )
        })
        .await)
}

#[tauri::command]
#[specta::specta]
pub async fn compute_horizon(
    app: tauri::AppHandle,
    dem_cache: tauri::State<'_, crate::dem::DemCache>,
    observer: Geodetic,
    max_range_m: f64,
) -> std::result::Result<Vec<(f64, f64)>, String> {
    compute_horizon_impl(&app, &dem_cache, observer, max_range_m)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn fetch_peaks(
    app: tauri::AppHandle,
    dem_cache: tauri::State<'_, crate::dem::DemCache>,
    peak_store: tauri::State<'_, crate::peakstore::PeakStore>,
    lat: f64,
    lon: f64,
    radius_m: f64,
) -> std::result::Result<Vec<Peak>, String> {
    fetch_peaks_impl(&app, &dem_cache, &peak_store, lat, lon, radius_m)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn get_elevation(
    app: tauri::AppHandle,
    dem_cache: tauri::State<'_, crate::dem::DemCache>,
    lat: f64,
    lon: f64,
) -> std::result::Result<f64, String> {
    get_elevation_impl(&app, &dem_cache, lat, lon)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn filter_visible_peaks(
    app: tauri::AppHandle,
    dem_cache: tauri::State<'_, crate::dem::DemCache>,
    observer: Geodetic,
    peaks: Vec<Peak>,
    radius_m: f64,
) -> std::result::Result<Vec<Peak>, String> {
    filter_visible_peaks_impl(&app, &dem_cache, observer, peaks, radius_m)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the committed dataset itself, not the code that reads it.
    ///
    /// The Tauri build script only checks that the declared resource *exists*, so a
    /// truncated or placeholder file builds and ships perfectly happily and simply
    /// returns no peaks on device. This is deliberately not `#[ignore]`d — it needs no
    /// network and it is the only thing standing between a bad regeneration and a
    /// release. Regenerate with `cargo run --release -p peaklab -- extract-peaks`.
    #[test]
    fn committed_dataset_is_real() {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/resources/peaks.mvpk"
        ))
        .expect("bundled peak dataset missing");
        let file = peakcore::peakfile::PeakFile::parse(bytes)
            .expect("bundled peak dataset is malformed");

        // Current scope is North America (~126k peaks); a placeholder or a truncated
        // write lands orders of magnitude below this.
        assert!(
            file.len() > 50_000,
            "dataset has only {} peaks — did a regeneration fail partway?",
            file.len()
        );

        // Mammoth Lakes: dense, well-mapped, and the area the AR overlay was first
        // debugged against, so a lookup here exercises real tile indexing.
        let near_mammoth = file
            .peaks_in_radius(37.65214, -118.98018, 100_000.0)
            .expect("querying the bundled dataset");
        assert!(
            near_mammoth.len() > 500,
            "expected hundreds of peaks within 100km of Mammoth, got {}",
            near_mammoth.len()
        );
        for name in ["Mount Morrison", "Bloody Mountain", "Laurel Mountain"] {
            assert!(
                near_mammoth.iter().any(|r| r.name == name),
                "expected {name} in the dataset near Mammoth"
            );
        }
    }

    /// End-to-end sanity check that peaks and observer elevation come from the same DEM
    /// — the property that broke when peak elevation used to come from Open-Elevation.
    ///
    /// Reads the committed dataset directly rather than going through [`PeakStore`],
    /// which resolves a bundled app resource that `mock_app` has no notion of. Still
    /// ignored by default because the DEM tiles themselves are fetched from S3; run with
    /// `cargo test -p mountain-view -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn peaks_and_elevation_use_the_same_dem() {
        let app = tauri::test::mock_app();
        let dem_cache = crate::dem::DemCache::default();

        // Paradise, Mount Rainier — well inside DEM coverage and near enough to the
        // mountain to guarantee named peaks in range.
        let (lat, lon) = (46.7857, -121.7353);

        let ground = get_elevation_impl(app.handle(), &dem_cache, lat, lon)
            .await
            .expect("no DEM coverage at Paradise");
        assert!(
            (1_400.0..2_200.0).contains(&ground),
            "expected Paradise's ~1,650 m elevation, got {ground}"
        );

        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/resources/peaks.mvpk"
        ))
        .expect("bundled peak dataset missing — run `peaklab extract-peaks`");
        let raw = peakcore::peakfile::PeakFile::parse(bytes)
            .expect("bundled peak dataset is malformed")
            .peaks_in_radius(lat, lon, 20_000.0)
            .expect("querying the bundled dataset");
        assert!(!raw.is_empty(), "expected named peaks within 20km of Rainier");

        let peaks = snap_to_dem(app.handle(), &dem_cache, raw, lat, lon, 20_000.0)
            .await
            .expect("failed to snap peaks to the DEM");
        assert!(!peaks.is_empty(), "every peak was dropped by the DEM snap");
        for peak in &peaks {
            assert!(
                peak.elev > 0.0,
                "expected a real DEM elevation for {}, got {}",
                peak.name,
                peak.elev
            );
        }
    }
}
