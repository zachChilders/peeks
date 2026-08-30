//! Named peaks (Overpass), snapped to and elevated from the same local Copernicus
//! GLO-30 DEM used for terrain occlusion — see [`crate::dem`] and [`peakcore::dem`].
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
use peakcore::overpass;
use peakcore::visibility::{self, VisibilityConfig};
use serde::{Deserialize, Serialize};
use specta::Type;
use specta_typescript::Number;

const OVERPASS_URL: &str = "https://overpass-api.de/api/interpreter";

/// DEM postings (~30 m) to search around each OSM peak node for the true local summit.
/// Matches peaklab's own default: measured against 344 peaks' tagged elevations near
/// Rainier, wider windows mostly climb onto neighbouring terrain rather than fixing
/// anything — the count of peaks reading >50 m above their tagged elevation roughly
/// triples from window=0 to window=240 m (see `peaklab/src/peaks.rs`).
const SNAP_HALF_WINDOW: i64 = 1;

/// Extra margin, in metres, when loading the DEM for a peak fetch beyond the Overpass
/// radius itself — guarantees the snap window has coverage even for a peak right at the
/// edge of that radius.
const SNAP_DEM_MARGIN_M: f64 = 2_000.0;

/// Mirrors peaklab's own client timeout. The previous mobile client
/// (src-tauri/src/overpass.rs, now folded into this module) set none at all, which
/// pinned the AR view at "Orienting..." forever on a hung Overpass request.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Azimuth spacing for the debug DEM-horizon skyline. Matches `visibility::check`'s own
/// along-path step for the ray-march distance step; this is the angular step of a full
/// 360° sweep around the observer instead.
const HORIZON_AZIMUTH_STEP_DEG: f64 = 2.0;
const HORIZON_RAY_STEP_M: f64 = 60.0;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("Overpass returned {0}")]
    OverpassStatus(reqwest::StatusCode),
    #[error(transparent)]
    OverpassParse(#[from] overpass::ParseError),
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

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        // Overpass rejects requests without a descriptive User-Agent (HTTP 406).
        .user_agent("mountain-view/0.1 (AR peak identification)")
        .build()?)
}

async fn fetch_named_peaks(
    client: &reqwest::Client,
    lat: f64,
    lon: f64,
    radius_m: f64,
) -> Result<Vec<overpass::RawPeak>> {
    let query = overpass::build_query(lat, lon, radius_m);

    let resp = client
        .post(OVERPASS_URL)
        .form(&[("data", query.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(Error::OverpassStatus(resp.status()));
    }

    let body = resp.text().await?;
    Ok(overpass::parse_response(&body)?)
}

async fn fetch_peaks_impl<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    dem_cache: &crate::dem::DemCache,
    lat: f64,
    lon: f64,
    radius_m: f64,
) -> Result<Vec<Peak>> {
    let client = http_client()?;
    let raw = fetch_named_peaks(&client, lat, lon, radius_m).await?;

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
            let mut out = Vec::new();
            let mut az = 0.0;
            while az < 360.0 {
                if let Some(el) =
                    visibility::horizon_at_azimuth(dem, observer, az, max_range_m, HORIZON_RAY_STEP_M)
                {
                    out.push((az, el));
                }
                az += HORIZON_AZIMUTH_STEP_DEG;
            }
            out
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
    lat: f64,
    lon: f64,
    radius_m: f64,
) -> std::result::Result<Vec<Peak>, String> {
    fetch_peaks_impl(&app, &dem_cache, lat, lon, radius_m)
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

    /// End-to-end sanity check against real Overpass and DEM data: this is the piece
    /// that changed when peak/observer elevation moved off Open-Elevation onto the same
    /// DEM the occlusion raycast uses. Ignored by default since it needs network access;
    /// run explicitly with `cargo test -p mountain-view -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn fetch_peaks_and_get_elevation_use_the_same_dem() {
        let app = tauri::test::mock_app();
        let dem_cache = crate::dem::DemCache::default();

        // Paradise, Mount Rainier — well inside DEM coverage and near enough to the
        // mountain to guarantee at least one named peak comes back from Overpass.
        let (lat, lon) = (46.7857, -121.7353);

        let ground = get_elevation_impl(app.handle(), &dem_cache, lat, lon)
            .await
            .expect("no DEM coverage at Paradise");
        assert!(
            (1_400.0..2_200.0).contains(&ground),
            "expected Paradise's ~1,650 m elevation, got {ground}"
        );

        let peaks = fetch_peaks_impl(app.handle(), &dem_cache, lat, lon, 20_000.0)
            .await
            .expect("failed to fetch peaks");
        assert!(!peaks.is_empty(), "expected at least one named peak near Rainier");
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
