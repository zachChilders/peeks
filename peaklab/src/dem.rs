//! Copernicus GLO-30 digital surface model access.
//!
//! Tiles are 1°×1°, 3600×3600 Float32 postings (1 arcsecond ≈ 30 m) on WGS84 lat/lon,
//! stored as DEFLATE COGs with a floating-point predictor. Postings sit on exact
//! arcsecond multiples, so it is convenient to index them in a single global grid:
//!
//! ```text
//! row = (90 - lat) * 3600     col = (lon + 180) * 3600
//! ```
//!
//! Tiles are resolved out of that global index, which makes bilinear sampling across a
//! tile seam fall out for free instead of being a special case.
//!
//! Note this is a *surface* model: over Rainier's summit ice cap it reads ~20 m above the
//! published rock/ice elevation. That is the DSM behaving correctly, not a sampling bug.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};

pub const TILE_DIM: i64 = 3600;
const S3_BASE: &str = "https://copernicus-dem-30m.s3.amazonaws.com";

/// A tile that is absent upstream (all-ocean cells) reads as sea level.
const SEA_LEVEL: f32 = 0.0;

fn tile_name(lat0: i32, lon0: i32) -> String {
    let (ns, lat_abs) = if lat0 >= 0 { ('N', lat0) } else { ('S', -lat0) };
    let (ew, lon_abs) = if lon0 >= 0 { ('E', lon0) } else { ('W', -lon0) };
    format!("Copernicus_DSM_COG_10_{ns}{lat_abs:02}_00_{ew}{lon_abs:03}_00_DEM")
}

pub struct Dem {
    dir: PathBuf,
    /// `None` means "confirmed absent upstream", so we do not retry the fetch.
    tiles: HashMap<(i32, i32), Option<Vec<f32>>>,
}

impl Dem {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            dir: dir.as_ref().to_path_buf(),
            tiles: HashMap::new(),
        }
    }

    /// Build a `Dem` directly from in-memory tiles, bypassing the filesystem/network.
    /// Test-only: lets visibility/geometry tests exercise real [`Dem`] sampling against
    /// hand-built synthetic terrain instead of hitting the actual DEM data.
    #[cfg(test)]
    pub fn from_tiles_for_test(tiles: Vec<((i32, i32), Vec<f32>)>) -> Self {
        Self {
            dir: PathBuf::new(),
            tiles: tiles.into_iter().map(|(k, v)| (k, Some(v))).collect(),
        }
    }

    /// Number of tiles currently resident, and their total size in MiB.
    pub fn resident(&self) -> (usize, f64) {
        let n = self.tiles.values().filter(|t| t.is_some()).count();
        let bytes = n as f64 * (TILE_DIM * TILE_DIM) as f64 * 4.0;
        (n, bytes / (1024.0 * 1024.0))
    }

    /// Load every tile touching a `radius_m` circle around `(lat, lon)`, downloading any
    /// that are missing.
    ///
    /// Preloading up front keeps [`Dem::elevation_at`] an immutable read, so the hot
    /// raycasting loop never has to take a lock or mutate the cache.
    pub fn load_region(&mut self, lat: f64, lon: f64, radius_m: f64) -> Result<()> {
        let dlat = radius_m / 111_320.0;
        // Guard the cosine so a high-latitude region does not ask for the whole globe.
        let dlon = radius_m / (111_320.0 * lat.to_radians().cos().abs().max(0.05));

        let lat_range = (lat - dlat, lat + dlat);
        let lon_range = (lon - dlon, lon + dlon);

        // Latitude rows run from the tile's north edge downward, so a tile whose south
        // edge is `lat0` covers (lat0, lat0 + 1].
        let lat0_min = (lat_range.0.ceil() as i32) - 1;
        let lat0_max = (lat_range.1.ceil() as i32) - 1;
        let lon0_min = lon_range.0.floor() as i32;
        let lon0_max = lon_range.1.floor() as i32;

        for lat0 in lat0_min..=lat0_max {
            for lon0 in lon0_min..=lon0_max {
                self.ensure_tile(lat0, lon0)?;
            }
        }
        Ok(())
    }

    fn ensure_tile(&mut self, lat0: i32, lon0: i32) -> Result<()> {
        if self.tiles.contains_key(&(lat0, lon0)) {
            return Ok(());
        }

        let name = tile_name(lat0, lon0);
        let path = self.dir.join(format!("{name}.tif"));

        if !path.exists() {
            match download_tile(&name, &path)? {
                true => {}
                false => {
                    eprintln!("  tile {name} absent upstream; treating as sea level");
                    self.tiles.insert((lat0, lon0), None);
                    return Ok(());
                }
            }
        }

        let grid = decode_tile(&path).with_context(|| format!("decoding {}", path.display()))?;
        self.tiles.insert((lat0, lon0), Some(grid));
        Ok(())
    }

    /// Raw posting value at a global grid index, or `None` if its tile is not resident.
    fn posting(&self, grow: i64, gcol: i64) -> Option<f32> {
        let trow = grow.div_euclid(TILE_DIM);
        let tcol = gcol.div_euclid(TILE_DIM);
        let lat0 = (89 - trow) as i32;
        let lon0 = (tcol - 180) as i32;

        match self.tiles.get(&(lat0, lon0)) {
            Some(Some(grid)) => {
                let row = grow - trow * TILE_DIM;
                let col = gcol - tcol * TILE_DIM;
                Some(grid[(row * TILE_DIM + col) as usize])
            }
            // Known-absent tile: open ocean.
            Some(None) => Some(SEA_LEVEL),
            None => None,
        }
    }

    /// Bilinearly interpolated surface elevation in metres.
    ///
    /// Nearest-neighbour sampling puts visible stair-steps into the horizon profile,
    /// which show up as false occlusions during the visibility raycast.
    pub fn elevation_at(&self, lat: f64, lon: f64) -> Option<f64> {
        let grf = (90.0 - lat) * TILE_DIM as f64;
        let gcf = (lon + 180.0) * TILE_DIM as f64;

        let gr0 = grf.floor() as i64;
        let gc0 = gcf.floor() as i64;
        let fr = grf - gr0 as f64;
        let fc = gcf - gc0 as f64;

        let v00 = self.posting(gr0, gc0)? as f64;
        let v01 = self.posting(gr0, gc0 + 1)? as f64;
        let v10 = self.posting(gr0 + 1, gc0)? as f64;
        let v11 = self.posting(gr0 + 1, gc0 + 1)? as f64;

        let top = v00 * (1.0 - fc) + v01 * fc;
        let bottom = v10 * (1.0 - fc) + v11 * fc;
        Some(top * (1.0 - fr) + bottom * fr)
    }

    /// Nearest posting without interpolation. Only useful for cross-checking the sampler.
    pub fn elevation_nearest(&self, lat: f64, lon: f64) -> Option<f64> {
        let grow = ((90.0 - lat) * TILE_DIM as f64).round() as i64;
        let gcol = ((lon + 180.0) * TILE_DIM as f64).round() as i64;
        self.posting(grow, gcol).map(|v| v as f64)
    }

    /// Highest posting within a `half_window`-posting square, returned as
    /// `(lat, lon, elevation)` of that posting's centre.
    ///
    /// OSM peak nodes are often placed a posting or two off the true local maximum;
    /// snapping to it keeps labels from drifting onto a neighbouring summit.
    pub fn local_max(&self, lat: f64, lon: f64, half_window: i64) -> Option<(f64, f64, f64)> {
        let gr = ((90.0 - lat) * TILE_DIM as f64).round() as i64;
        let gc = ((lon + 180.0) * TILE_DIM as f64).round() as i64;

        let mut best: Option<(f64, f64, f64)> = None;
        for dr in -half_window..=half_window {
            for dc in -half_window..=half_window {
                let Some(v) = self.posting(gr + dr, gc + dc) else {
                    continue;
                };
                let v = v as f64;
                if best.is_none_or(|(_, _, bv)| v > bv) {
                    let plat = 90.0 - (gr + dr) as f64 / TILE_DIM as f64;
                    let plon = (gc + dc) as f64 / TILE_DIM as f64 - 180.0;
                    best = Some((plat, plon, v));
                }
            }
        }
        best
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

fn decode_tile(path: &Path) -> Result<Vec<f32>> {
    let file = std::fs::File::open(path)?;
    let mut decoder = tiff::decoder::Decoder::new(BufReader::new(file))?;

    let (w, h) = decoder.dimensions()?;
    if w as i64 != TILE_DIM || h as i64 != TILE_DIM {
        bail!("expected {TILE_DIM}x{TILE_DIM} tile, got {w}x{h}");
    }

    match decoder.read_image()? {
        tiff::decoder::DecodingResult::F32(v) => Ok(v),
        _ => bail!("expected Float32 samples"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_naming() {
        assert_eq!(
            tile_name(46, -122),
            "Copernicus_DSM_COG_10_N46_00_W122_00_DEM"
        );
        assert_eq!(
            tile_name(-34, 18),
            "Copernicus_DSM_COG_10_S34_00_E018_00_DEM"
        );
    }

    /// The global-index maths must land on the tile and posting we expect, including
    /// at the seams where a degree boundary belongs to the neighbouring tile.
    #[test]
    fn global_index_resolves_expected_tile() {
        let resolve = |lat: f64, lon: f64| {
            let grow = ((90.0 - lat) * TILE_DIM as f64).round() as i64;
            let gcol = ((lon + 180.0) * TILE_DIM as f64).round() as i64;
            let trow = grow.div_euclid(TILE_DIM);
            let tcol = gcol.div_euclid(TILE_DIM);
            (
                (89 - trow) as i32,
                (tcol - 180) as i32,
                grow - trow * TILE_DIM,
                gcol - tcol * TILE_DIM,
            )
        };

        // Columbia Crest, cross-checked against gdallocationinfo.
        assert_eq!(resolve(46.852947, -121.760424), (46, -122, 529, 862));
        // A whole-degree latitude belongs to the tile *below* it, since rows run down
        // from the north edge.
        assert_eq!(resolve(46.0, -121.5).0, 45);
        // A whole-degree longitude belongs to the tile to its east.
        assert_eq!(resolve(46.5, -121.0).1, -121);
    }
}
