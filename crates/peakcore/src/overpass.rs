//! Overpass query construction and response parsing — the part of "fetch named peaks
//! from OSM" that has nothing to do with which HTTP client does the fetching.
//!
//! `peaklab` (blocking `reqwest` 0.12) and `src-tauri` (async `reqwest` 0.13) each own
//! their own transport rather than sharing one, to avoid forcing a `reqwest` version
//! unification across a desktop CLI and a mobile app. This module is the part that *can*
//! be shared: the query string and the parse of Overpass's JSON into [`RawPeak`].

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Overpass's own server-side query-processing budget, in seconds. Distinct from (and
/// much shorter than) the HTTP client timeout each caller sets on the request itself.
const OVERPASS_QUERY_TIMEOUT_S: u32 = 90;

/// Build an Overpass QL query for named `natural=peak` nodes within `radius_m` of a
/// point.
pub fn build_query(lat: f64, lon: f64, radius_m: f64) -> String {
    format!(
        "[out:json][timeout:{OVERPASS_QUERY_TIMEOUT_S}];\nnode[\"natural\"=\"peak\"][\"name\"](around:{radius_m:.0},{lat},{lon});\nout body;"
    )
}

/// Build an Overpass QL query for named `natural=peak` nodes in a bounding box, given as
/// `(south, west, north, east)` in degrees.
///
/// Used only by the one-time bulk extract that builds the bundled dataset
/// ([`crate::peakfile`]); the app queries that file, not Overpass. A bbox is the right
/// shape for tiling the globe, where [`build_query`]'s `around:` disc is the right shape
/// for "what can I see from here".
/// Gets a far larger server-side budget than [`OVERPASS_QUERY_TIMEOUT_S`]: a measured
/// 4°×10° cell over the Alps — the densest peak region on Earth — needed 98 s of server
/// time to return its 58,740 peaks, which the interactive 90 s budget would have killed.
/// Only the one-time extract pays this, never a user.
pub fn build_bbox_query(south: f64, west: f64, north: f64, east: f64) -> String {
    const BULK_QUERY_TIMEOUT_S: u32 = 300;
    format!(
        "[out:json][timeout:{BULK_QUERY_TIMEOUT_S}];\nnode[\"natural\"=\"peak\"][\"name\"]({south},{west},{north},{east});\nout body;"
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawPeak {
    pub osm_id: i64,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub ele: Option<f64>,
}

#[derive(Deserialize)]
struct OverpassResponse {
    elements: Vec<OverpassNode>,
}

#[derive(Deserialize)]
struct OverpassNode {
    id: i64,
    lat: f64,
    lon: f64,
    #[serde(default)]
    tags: HashMap<String, String>,
}

/// Error parsing an Overpass response body.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("invalid Overpass JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Parse an Overpass `[out:json]` response body into named peaks. Nodes without a
/// `name` tag are dropped — the query already filters on `["name"]`, but a defensive
/// filter here keeps this function correct even fed a hand-built or looser query.
pub fn parse_response(body: &str) -> Result<Vec<RawPeak>, ParseError> {
    let parsed: OverpassResponse = serde_json::from_str(body)?;
    Ok(parsed
        .elements
        .into_iter()
        .filter_map(|n| {
            Some(RawPeak {
                osm_id: n.id,
                lat: n.lat,
                lon: n.lon,
                name: n.tags.get("name")?.clone(),
                ele: n.tags.get("ele").and_then(|e| parse_ele(e)),
            })
        })
        .collect())
}

/// `ele` is free-form in practice: bare numbers, `"1234 m"`, occasionally junk.
fn parse_ele(s: &str) -> Option<f64> {
    let cleaned: String = s
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    cleaned.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ele_parsing() {
        assert_eq!(parse_ele("4392"), Some(4392.0));
        assert_eq!(parse_ele("4392.5 m"), Some(4392.5));
        assert_eq!(parse_ele(" 1234m"), Some(1234.0));
        assert_eq!(parse_ele("approx"), None);
    }

    #[test]
    fn build_query_embeds_params() {
        let q = build_query(46.85, -121.76, 50_000.0);
        assert!(q.contains("timeout:90"));
        assert!(q.contains("around:50000,46.85,-121.76"));
    }

    #[test]
    fn parse_response_skips_unnamed_and_reads_tags() {
        let body = r#"{
            "elements": [
                {"type": "node", "id": 1, "lat": 46.8, "lon": -121.7, "tags": {"name": "Test Peak", "ele": "1234"}},
                {"type": "node", "id": 2, "lat": 46.9, "lon": -121.8}
            ]
        }"#;
        let peaks = parse_response(body).unwrap();
        assert_eq!(peaks.len(), 1);
        assert_eq!(peaks[0].name, "Test Peak");
        assert_eq!(peaks[0].osm_id, 1);
        assert_eq!(peaks[0].ele, Some(1234.0));
    }
}
