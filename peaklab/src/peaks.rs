//! Named peaks from OpenStreetMap (M2).
//!
//! OSM's `natural=peak` nodes stand in for an offline prominence database: anything
//! worth labelling in a populated mountain range is already curated there, and the
//! editors have effectively done the "is this worth naming" filtering for us.
//!
//! Two things about the data are worth knowing:
//! - `tags.ele` is inconsistently populated and occasionally in feet, so it is recorded
//!   for reference but never used. Elevations always come from the DEM.
//! - The plan's assumption that nodes need snapping to the true local maximum did not
//!   hold up: measured against 344 peaks with a usable `ele` tag near Rainier, OSM node
//!   placement already agrees with the DEM at a median of ~11 m with *no* snapping.
//!   Widening the snap window doesn't improve that (still ~11 m median at window=240 m)
//!   but does increasingly climb onto neighbouring terrain — peaks reading >50 m above
//!   their tagged elevation roughly triple from window=0 to window=240 m. Gibraltar
//!   Rock is the poster child: an 8-posting window "snaps" it 296 m sideways onto
//!   Rainier's flank, reading 67 m too high. Default snap window is 1 posting (~30 m),
//!   which just guards against true off-by-one placement without inviting that failure
//!   mode.

use anyhow::{Context, Result};
use peakcore::overpass::{self, RawPeak};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::dem::Dem;

const OVERPASS_URL: &str = "https://overpass-api.de/api/interpreter";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peak {
    pub name: String,
    pub osm_id: i64,
    /// Snapped to the highest nearby DEM posting.
    pub lat: f64,
    pub lon: f64,
    /// DEM surface elevation at the snapped position, metres.
    pub elev: f64,
    /// The node position as tagged in OSM, before snapping.
    pub osm_lat: f64,
    pub osm_lon: f64,
    /// `tags.ele` as tagged, recorded for comparison only.
    pub osm_ele: Option<f64>,
    /// Distance from the OSM node to the snapped summit, metres.
    pub snap_offset_m: f64,
}

/// Fetch named peaks within `radius_m`, caching the raw Overpass response.
///
/// Overpass rate-limits aggressively and this gets re-run constantly during tuning, so
/// the cache is keyed on the rounded query parameters and reused unconditionally.
pub fn fetch_raw(cache_dir: &Path, lat: f64, lon: f64, radius_m: f64) -> Result<Vec<RawPeak>> {
    std::fs::create_dir_all(cache_dir)?;
    let key = format!(
        "peaks_{:.2}_{:.2}_{}km.json",
        lat,
        lon,
        (radius_m / 1000.0).round() as i64
    );
    let cache_path = cache_dir.join(key);

    if cache_path.exists() {
        let text = std::fs::read_to_string(&cache_path)?;
        return Ok(serde_json::from_str(&text)?);
    }

    let query = overpass::build_query(lat, lon, radius_m);
    eprintln!("  querying Overpass ({:.0} km radius) …", radius_m / 1000.0);

    // Overpass expects the query form-encoded as `data=`, and rejects requests without a
    // descriptive User-Agent.
    let resp = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(180))
        .user_agent("peaklab/0.1 (AR peak identification; contact via repo)")
        .build()?
        .post(OVERPASS_URL)
        .form(&[("data", query.as_str())])
        .send()
        .context("Overpass request failed")?
        .error_for_status()
        .context("Overpass returned an error status")?;

    let body = resp.text().context("reading Overpass response")?;
    let raw = overpass::parse_response(&body).context("parsing Overpass response")?;

    std::fs::write(&cache_path, serde_json::to_string_pretty(&raw)?)?;
    Ok(raw)
}

/// Attach DEM elevations, snapping each node to the highest posting within
/// `snap_half_window` postings (1 posting ≈ 30 m).
pub fn resolve(dem: &Dem, raw: &[RawPeak], snap_half_window: i64) -> Vec<Peak> {
    raw.iter()
        .filter_map(|r| {
            let (lat, lon, elev) = dem.local_max(r.lat, r.lon, snap_half_window)?;
            let offset = crate::geo::great_circle_distance(
                crate::geo::Geodetic::new(r.lat, r.lon, 0.0),
                crate::geo::Geodetic::new(lat, lon, 0.0),
            );
            Some(Peak {
                name: r.name.clone(),
                osm_id: r.osm_id,
                lat,
                lon,
                elev,
                osm_lat: r.lat,
                osm_lon: r.lon,
                osm_ele: r.ele,
                snap_offset_m: offset,
            })
        })
        .collect()
}

/// Convenience: fetch, load the DEM region, and resolve in one step.
pub fn load(
    data_dir: &Path,
    dem: &mut Dem,
    lat: f64,
    lon: f64,
    radius_m: f64,
    snap_half_window: i64,
) -> Result<Vec<Peak>> {
    let raw = fetch_raw(&cache_dir(data_dir), lat, lon, radius_m)?;
    dem.load_region(lat, lon, radius_m + 2_000.0)?;
    Ok(resolve(dem, &raw, snap_half_window))
}

pub fn cache_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("osm")
}
