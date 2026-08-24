//! Proxies the Overpass API query through Rust instead of the webview's `fetch()`.
//!
//! Overpass rejects requests without a descriptive `User-Agent` (HTTP 406) — see
//! peakcore's overpass module docs and peaklab's peaks.rs for where this was first hit
//! from the desktop tool. Browser `fetch()` can't set that header at all (`User-Agent`
//! is on the Fetch spec's forbidden header list, silently stripped by every browser
//! including WKWebView), so unlike the Open-Elevation calls elsewhere in this app, this
//! one has to go through Rust.

use peakcore::overpass;

#[tauri::command]
#[specta::specta]
pub async fn fetch_peaks_overpass(lat: f64, lon: f64, radius_m: f64) -> Result<String, String> {
    let query = overpass::build_query(lat, lon, radius_m);

    let client = reqwest::Client::builder()
        .user_agent("mountain-view/0.1 (AR peak identification)")
        .build()
        .map_err(|e| e.to_string())?;

    let res = client
        .post("https://overpass-api.de/api/interpreter")
        .form(&[("data", query.as_str())])
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !res.status().is_success() {
        return Err(format!("Overpass returned {}", res.status()));
    }

    res.text().await.map_err(|e| e.to_string())
}
