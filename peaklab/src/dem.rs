//! Blocking fetch and on-disk cache of Copernicus GLO-30 DEM tiles.
//!
//! Tile indexing, bilinear sampling, and decoding live in [`peakcore::dem`] (shared with
//! the mobile app's async fetch path); this module only adds the desktop-specific parts:
//! a blocking HTTP client and a directory of cached `.tif` files.

use anyhow::{bail, Context, Result};
use peakcore::dem;
use std::path::{Path, PathBuf};

const S3_BASE: &str = "https://copernicus-dem-30m.s3.amazonaws.com";

pub struct Dem {
    dir: PathBuf,
    inner: dem::Dem,
}

impl Dem {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
            inner: dem::Dem::new(),
        }
    }

    /// Build a `Dem` directly from in-memory tiles, bypassing the filesystem/network.
    /// Test-only: lets visibility/geometry tests exercise real [`Dem`] sampling against
    /// hand-built synthetic terrain instead of hitting the actual DEM data.
    #[cfg(test)]
    pub fn from_tiles_for_test(tiles: Vec<((i32, i32), Vec<f32>)>) -> Self {
        let mut inner = dem::Dem::new();
        for (k, grid) in tiles {
            inner.insert_tile(k.0, k.1, Some(grid));
        }
        Self {
            dir: PathBuf::new(),
            inner,
        }
    }

    /// Number of tiles currently resident, and their total size in MiB.
    pub fn resident(&self) -> (usize, f64) {
        self.inner.resident()
    }

    /// Load every tile touching a `radius_m` circle around `(lat, lon)`, downloading any
    /// that are missing.
    ///
    /// Preloading up front keeps [`Dem::elevation_at`] an immutable read, so the hot
    /// raycasting loop never has to take a lock or mutate the cache.
    pub fn load_region(&mut self, lat: f64, lon: f64, radius_m: f64) -> Result<()> {
        for (lat0, lon0) in dem::tiles_for_region(lat, lon, radius_m) {
            self.ensure_tile(lat0, lon0)?;
        }
        Ok(())
    }

    fn ensure_tile(&mut self, lat0: i32, lon0: i32) -> Result<()> {
        if self.inner.has_tile(lat0, lon0) {
            return Ok(());
        }

        let name = dem::tile_name(lat0, lon0);
        let path = self.dir.join(format!("{name}.tif"));

        if !path.exists() && !download_tile(&name, &path)? {
            eprintln!("  tile {name} absent upstream; treating as sea level");
            self.inner.insert_tile(lat0, lon0, None);
            return Ok(());
        }

        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let grid = dem::decode_tile(&bytes).with_context(|| format!("decoding {}", path.display()))?;
        self.inner.insert_tile(lat0, lon0, Some(grid));
        Ok(())
    }

    /// Bilinearly interpolated surface elevation in metres.
    pub fn elevation_at(&self, lat: f64, lon: f64) -> Option<f64> {
        self.inner.elevation_at(lat, lon)
    }

    /// Nearest posting without interpolation. Only useful for cross-checking the sampler.
    pub fn elevation_nearest(&self, lat: f64, lon: f64) -> Option<f64> {
        self.inner.elevation_nearest(lat, lon)
    }

    /// Highest posting within a `half_window`-posting square, returned as
    /// `(lat, lon, elevation)` of that posting's centre.
    pub fn local_max(&self, lat: f64, lon: f64, half_window: i64) -> Option<(f64, f64, f64)> {
        self.inner.local_max(lat, lon, half_window)
    }

    /// The shared, transport-free [`peakcore::dem::Dem`] backing this cache — what
    /// [`peakcore::visibility::check`] and `profile` actually sample against.
    pub fn core(&self) -> &dem::Dem {
        &self.inner
    }
}

/// Returns `false` if the tile does not exist upstream (404).
fn download_tile(name: &str, dest: &Path) -> Result<bool> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let url = format!("{S3_BASE}/{name}/{name}.tif");
    eprintln!("  fetching {name} …");

    let resp = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()?
        .get(&url)
        .send()
        .with_context(|| format!("requesting {url}"))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(false);
    }
    if !resp.status().is_success() {
        bail!("{url} returned {}", resp.status());
    }

    let bytes = resp.bytes()?;
    // Write via a temp file so an interrupted run cannot leave a truncated tile that
    // later looks cached.
    let tmp = dest.with_extension("tif.part");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, dest)?;
    Ok(true)
}
