//! Proxies the Overpass API query through Rust instead of the webview's `fetch()`.
//!
//! Overpass rejects requests without a descriptive `User-Agent` (HTTP 406) — see
//! peaklab's peaks.rs for where this was first hit from the desktop tool. Browser
//! `fetch()` can't set that header at all (`User-Agent` is on the Fetch spec's forbidden
//! header list, silently stripped by every browser including WKWebView), so unlike the
//! Open-Elevation calls elsewhere in this app, this one has to go through Rust.

#[tauri::command]
pub async fn fetch_peaks_overpass(lat: f64, lon: f64, radius_m: f64) -> Result<String, String> {
    let query = format!(
        "[out:json][timeout:60];\nnode[\"natural\"=\"peak\"][\"name\"](around:{radius_m},{lat},{lon});\nout body;"
    );

    let client = reqwest::Client::builder()
        .user_agent("mountain-view/0.1 (AR peak identification)")
        .build()
        .map_err(|e| e.to_string())?;

    let res = client
        .post("https://overpass-api.de/api/interpreter")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("data={}", urlencoding_query(&query)))
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !res.status().is_success() {
        return Err(format!("Overpass returned {}", res.status()));
    }

    res.text().await.map_err(|e| e.to_string())
}

/// Minimal percent-encoding for a `data=` form field — avoids pulling in a whole crate
/// just for this one call site.
fn urlencoding_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}
