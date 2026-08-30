//! Named peaks (Overpass) and their elevations (Open-Elevation, SRTM-backed), fetched
//! and assembled entirely in Rust.
//!
//! Consolidates what `src/lib/peaks.ts` did in TypeScript, plus the `fetchElevation`
//! helper that `MapView.tsx` and `CameraView.tsx` each duplicated verbatim for their own
//! single-point lookups — both now call the same [`get_elevation`] command.

use peakcore::geo::{self, Geodetic};
use peakcore::overpass;
use serde::{Deserialize, Serialize};
use specta::Type;
use specta_typescript::Number;

const OVERPASS_URL: &str = "https://overpass-api.de/api/interpreter";
const ELEVATION_URL: &str = "https://api.open-elevation.com/api/v1/lookup";
const ELEVATION_BATCH_SIZE: usize = 100;

/// Fractional distances (observer to peak) sampled for the terrain horizon check.
/// Front-loaded: a given height of intervening terrain blocks a larger angle the closer
/// it is to the observer, so nearby terrain needs finer sampling than far terrain.
const OCCLUSION_SAMPLE_FRACS: &[f64] =
    &[0.02, 0.05, 0.10, 0.18, 0.28, 0.40, 0.53, 0.66, 0.78, 0.88, 0.95];

/// Mirrors peaklab's own client timeout. The previous mobile client
/// (src-tauri/src/overpass.rs, now folded into this module) set none at all, which
/// pinned the AR view at "Orienting..." forever on a hung Overpass request.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("Overpass returned {0}")]
    OverpassStatus(reqwest::StatusCode),
    #[error(transparent)]
    OverpassParse(#[from] overpass::ParseError),
    #[error("Open-Elevation returned {0}")]
    ElevationStatus(reqwest::StatusCode),
    #[error("Open-Elevation returned {got} elevations for {expected} points")]
    ElevationBatchMismatch { expected: usize, got: usize },
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct Peak {
    pub name: String,
    // See scene.rs's PeakWithMetrics::osm_id for why this needs the Number override.
    #[specta(type = Number)]
    pub osm_id: i64,
    pub lat: f64,
    pub lon: f64,
    /// Open-Elevation (SRTM-backed) elevation in metres.
    pub elev: f64,
}

fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        // Overpass rejects requests without a descriptive User-Agent (HTTP 406); Open-
        // Elevation doesn't require one, but sending it anyway costs nothing.
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

#[derive(Serialize)]
struct ElevationLocation {
    latitude: f64,
    longitude: f64,
}

#[derive(Serialize)]
struct ElevationRequest<'a> {
    locations: &'a [ElevationLocation],
}

#[derive(Deserialize)]
struct ElevationResult {
    elevation: f64,
}

#[derive(Deserialize)]
struct ElevationResponse {
    results: Vec<ElevationResult>,
}

/// Batched Open-Elevation lookup, matching `points`' order. Errors (rather than
/// silently defaulting to sea level) if a batch's response is shorter than what was
/// asked for — the bug this replaces filled the output array with `0` upfront and only
/// overwrote the indices the API actually returned.
async fn fetch_elevations(client: &reqwest::Client, points: &[(f64, f64)]) -> Result<Vec<f64>> {
    let mut out = Vec::with_capacity(points.len());

    for batch in points.chunks(ELEVATION_BATCH_SIZE) {
        let locations: Vec<ElevationLocation> = batch
            .iter()
            .map(|&(lat, lon)| ElevationLocation { latitude: lat, longitude: lon })
            .collect();

        let resp = client
            .post(ELEVATION_URL)
            .json(&ElevationRequest { locations: &locations })
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(Error::ElevationStatus(resp.status()));
        }

        let body: ElevationResponse = resp.json().await?;
        if body.results.len() != batch.len() {
            return Err(Error::ElevationBatchMismatch {
                expected: batch.len(),
                got: body.results.len(),
            });
        }
        out.extend(body.results.into_iter().map(|r| r.elevation));
    }

    Ok(out)
}

async fn fetch_peaks_impl(lat: f64, lon: f64, radius_m: f64) -> Result<Vec<Peak>> {
    let client = http_client()?;

    let raw = fetch_named_peaks(&client, lat, lon, radius_m).await?;
    let points: Vec<(f64, f64)> = raw.iter().map(|p| (p.lat, p.lon)).collect();
    let elevations = fetch_elevations(&client, &points).await?;

    Ok(raw
        .into_iter()
        .zip(elevations)
        .map(|(p, elev)| Peak {
            name: p.name,
            osm_id: p.osm_id,
            lat: p.lat,
            lon: p.lon,
            elev,
        })
        .collect())
}

async fn get_elevation_impl(lat: f64, lon: f64) -> Result<f64> {
    let client = http_client()?;
    let elevations = fetch_elevations(&client, &[(lat, lon)]).await?;
    Ok(elevations[0])
}

/// Drops peaks whose line of sight from `observer` is blocked by intervening terrain.
///
/// For each peak, samples ground elevation at [`OCCLUSION_SAMPLE_FRACS`] points along
/// the path to it and compares each sample's apparent elevation angle (via
/// [`geo::look_angles`], which already folds in Earth curvature and refraction) against
/// the peak's own. A sample that appears higher than the peak sits in front of it and
/// blocks the view. All samples for all peaks are batched into one set of
/// [`fetch_elevations`] calls rather than one round trip per peak.
async fn filter_visible_peaks_impl(observer: Geodetic, peaks: Vec<Peak>) -> Result<Vec<Peak>> {
    let client = http_client()?;
    let n = OCCLUSION_SAMPLE_FRACS.len();

    let sample_points: Vec<(f64, f64)> = peaks
        .iter()
        .flat_map(|peak| {
            let peak_geo = Geodetic::new(peak.lat, peak.lon, peak.elev);
            OCCLUSION_SAMPLE_FRACS
                .iter()
                .map(move |&frac| geo::great_circle_point(observer, peak_geo, frac))
        })
        .collect();

    let elevations = fetch_elevations(&client, &sample_points).await?;

    Ok(peaks
        .into_iter()
        .enumerate()
        .filter(|(i, peak)| {
            let peak_geo = Geodetic::new(peak.lat, peak.lon, peak.elev);
            let (_, peak_elev, _) = geo::look_angles(observer, peak_geo);
            let start = i * n;
            !sample_points[start..start + n]
                .iter()
                .zip(&elevations[start..start + n])
                .any(|(&(lat, lon), &elev)| {
                    geo::look_angles(observer, Geodetic::new(lat, lon, elev)).1 > peak_elev
                })
        })
        .map(|(_, peak)| peak)
        .collect())
}

#[tauri::command]
#[specta::specta]
pub async fn fetch_peaks(lat: f64, lon: f64, radius_m: f64) -> std::result::Result<Vec<Peak>, String> {
    fetch_peaks_impl(lat, lon, radius_m).await.map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn get_elevation(lat: f64, lon: f64) -> std::result::Result<f64, String> {
    get_elevation_impl(lat, lon).await.map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn filter_visible_peaks(
    observer: Geodetic,
    peaks: Vec<Peak>,
) -> std::result::Result<Vec<Peak>, String> {
    filter_visible_peaks_impl(observer, peaks).await.map_err(|e| e.to_string())
}
