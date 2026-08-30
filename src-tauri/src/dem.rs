//! Async fetch and on-disk cache of Copernicus GLO-30 DEM tiles, backing the on-device
//! terrain-occlusion check in `peaks::filter_visible_peaks`.
//!
//! Tile indexing, bilinear sampling, and decoding live in `peakcore::dem` (shared with
//! peaklab's blocking fetch path — see that crate's module doc for why fetching itself
//! isn't shared); this module adds the async HTTP client, the app's cache directory, and
//! a lock so concurrent scene loads share one cache instead of re-downloading.

use peakcore::dem;
use std::path::PathBuf;
use tauri::Manager;
use tokio::sync::Mutex;

const S3_BASE: &str = "https://copernicus-dem-30m.s3.amazonaws.com";
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("tile fetch returned {0}")]
    Status(reqwest::StatusCode),
    #[error("tile decode failed: {0}")]
    Decode(#[from] dem::DecodeError),
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not resolve the app cache directory: {0}")]
    Tauri(#[from] tauri::Error),
    #[error("tile decode task panicked: {0}")]
    Join(#[from] tokio::task::JoinError),
}

#[derive(Default)]
struct CacheInner {
    dir: Option<PathBuf>,
    dem: dem::Dem,
}

/// Tauri-managed state: the shared local DEM cache, resident for the app's lifetime.
///
/// A `tokio::sync::Mutex` rather than `std::sync::Mutex`: [`DemCache::load_region`] holds
/// it across `.await` points (network fetches, decode), which a std mutex guard can't do.
#[derive(Default)]
pub struct DemCache(Mutex<CacheInner>);

impl DemCache {
    /// Ensure every tile touching a `radius_m` circle around `(lat, lon)` is loaded into
    /// the in-memory cache, downloading (and disk-caching under the app's cache dir) any
    /// that are missing.
    pub async fn load_region<R: tauri::Runtime>(
        &self,
        app: &tauri::AppHandle<R>,
        lat: f64,
        lon: f64,
        radius_m: f64,
    ) -> Result<()> {
        let mut guard = self.0.lock().await;

        if guard.dir.is_none() {
            let dir = app.path().app_cache_dir()?.join("dem");
            tokio::fs::create_dir_all(&dir).await?;
            guard.dir = Some(dir);
        }
        let dir = guard.dir.clone().expect("just set above");

        for (lat0, lon0) in dem::tiles_for_region(lat, lon, radius_m) {
            if guard.dem.has_tile(lat0, lon0) {
                continue;
            }

            let name = dem::tile_name(lat0, lon0);
            let path = dir.join(format!("{name}.tif"));

            let bytes = if let Ok(bytes) = tokio::fs::read(&path).await {
                bytes
            } else {
                match fetch_tile(&name).await? {
                    Some(bytes) => {
                        // Write via a temp file so an interrupted run cannot leave a
                        // truncated tile that a later launch mistakes for cached.
                        let tmp = path.with_extension("tif.part");
                        tokio::fs::write(&tmp, &bytes).await?;
                        tokio::fs::rename(&tmp, &path).await?;
                        bytes
                    }
                    None => {
                        guard.dem.insert_tile(lat0, lon0, None);
                        continue;
                    }
                }
            };

            // Decoding is CPU-bound (DEFLATE + float unpacking over ~50 MB), so it goes
            // through spawn_blocking rather than tying up the async executor.
            let grid = tokio::task::spawn_blocking(move || dem::decode_tile(&bytes)).await??;
            guard.dem.insert_tile(lat0, lon0, Some(grid));
        }

        Ok(())
    }

    /// Run `f` against the current DEM snapshot while holding the cache lock. Callers
    /// should have already awaited [`load_region`](Self::load_region) for the area they
    /// need — this does not fetch anything itself.
    pub async fn with_dem<T>(&self, f: impl FnOnce(&dem::Dem) -> T) -> T {
        let guard = self.0.lock().await;
        f(&guard.dem)
    }
}

/// Returns `Ok(None)` if the tile does not exist upstream (404) — treated as sea level.
async fn fetch_tile(name: &str) -> Result<Option<Vec<u8>>> {
    let url = format!("{S3_BASE}/{name}/{name}.tif");

    let resp = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()?
        .get(&url)
        .send()
        .await?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(Error::Status(resp.status()));
    }

    Ok(Some(resp.bytes().await?.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end sanity check against the real S3 bucket and a real mountain: the async
    /// fetch/cache plumbing (app cache dir resolution, tokio::fs, spawn_blocking decode)
    /// is new for this port and untested by peakcore's own unit tests, which only
    /// exercise the pure math against synthetic tiles. Ignored by default since it needs
    /// network access; run explicitly with `cargo test -p mountain-view -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn loads_real_tile_and_samples_a_known_summit() {
        let app = tauri::test::mock_app();
        let cache = DemCache::default();

        // Columbia Crest, Mount Rainier — cross-checked against gdallocationinfo in
        // peaklab's own dem tests, so any regression here is in the async plumbing, not
        // the tile-index math it shares with peaklab.
        let (lat, lon) = (46.852947, -121.760424);
        cache
            .load_region(app.handle(), lat, lon, 2_000.0)
            .await
            .expect("failed to load DEM region");

        let elev = cache.with_dem(|dem| dem.elevation_at(lat, lon)).await;
        assert!(
            matches!(elev, Some(e) if e > 4_000.0),
            "expected a summit elevation near Columbia Crest, got {elev:?}"
        );
    }
}
