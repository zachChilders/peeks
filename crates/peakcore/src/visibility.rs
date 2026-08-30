//! Line-of-sight visibility via terrain raycasting (M3).
//!
//! Walks the great-circle path from observer to target in fixed steps, and asks at each
//! step: does the terrain there poke up above the line to the target? If any sample does,
//! the target is occluded.
//!
//! Reuses the exact ECEF elevation-angle machinery from [`crate::geo`] for both the
//! target and every terrain sample, rather than switching to the flat/spherical
//! approximation the plan sketches — same accuracy, and it was already built and tested
//! for the bearing calculation, so there is no second, less-trustworthy code path to
//! keep consistent with the first.

use crate::dem::Dem;
use crate::geo::{self, Geodetic};

#[derive(Debug, Clone, Copy)]
pub struct VisibilityConfig {
    /// Distance between terrain samples along the path, metres. The plan's 60 m
    /// (≈2× the 30 m DEM posting) balances catching real ridgelines against sampling
    /// noise between postings; see `visibility_step_convergence` for the measurement.
    pub step_m: f64,
    /// Slack added to the target's angle before comparing, in degrees, so a peak is not
    /// occluded by samples on its own summit (path samples never land exactly on the
    /// target, but can land a posting or two away at nearly the same angle).
    pub tolerance_deg: f64,
}

impl Default for VisibilityConfig {
    fn default() -> Self {
        Self {
            step_m: 60.0,
            tolerance_deg: 0.1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Visibility {
    Visible,
    /// Blocked by terrain at the given fraction of the path (0..1) and distance (m).
    Occluded { at_frac: f64, at_dist_m: f64 },
    /// A terrain sample along the path had no DEM coverage.
    Unknown,
}

impl Visibility {
    pub fn is_visible(&self) -> bool {
        matches!(self, Visibility::Visible)
    }
}

/// Apparent elevation angle (geometric + refraction) of a point as seen from `observer`,
/// in degrees.
fn apparent_elevation_deg(observer: Geodetic, point: Geodetic) -> f64 {
    let v = geo::enu(observer, point);
    geo::elevation_deg(v) + geo::refraction_lift_deg(geo::horizontal_range(v))
}

/// Is `target` visible from `observer`, accounting for intervening terrain?
pub fn check(
    dem: &Dem,
    observer: Geodetic,
    target: Geodetic,
    cfg: VisibilityConfig,
) -> Visibility {
    let target_angle = apparent_elevation_deg(observer, target) + cfg.tolerance_deg;
    let d_total = geo::great_circle_distance(observer, target);

    let mut d = cfg.step_m;
    while d < d_total - cfg.step_m {
        let frac = d / d_total;
        let (slat, slon) = geo::great_circle_point(observer, target, frac);

        let Some(selev) = dem.elevation_at(slat, slon) else {
            return Visibility::Unknown;
        };
        let sample = Geodetic::new(slat, slon, selev);
        let sample_angle = apparent_elevation_deg(observer, sample);

        if sample_angle > target_angle {
            return Visibility::Occluded {
                at_frac: frac,
                at_dist_m: d,
            };
        }
        d += cfg.step_m;
    }
    Visibility::Visible
}

/// The horizon profile along a path: apparent elevation angle at each sampled distance.
/// Exposed for plotting/debugging — not used by [`check`] itself, which short-circuits.
pub fn profile(
    dem: &Dem,
    observer: Geodetic,
    target: Geodetic,
    step_m: f64,
) -> Vec<(f64, Option<f64>)> {
    let d_total = geo::great_circle_distance(observer, target);
    let mut out = Vec::new();
    let mut d = step_m;
    while d < d_total {
        let frac = d / d_total;
        let (slat, slon) = geo::great_circle_point(observer, target, frac);
        let angle = dem
            .elevation_at(slat, slon)
            .map(|e| apparent_elevation_deg(observer, Geodetic::new(slat, slon, e)));
        out.push((d, angle));
        d += step_m;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic-terrain sanity check: a target directly behind a taller ridge must be
    /// occluded, and the same target with the ridge removed must be visible. Uses a
    /// hand-built single-tile [`Dem`] so the test has no network/filesystem dependency.
    #[test]
    fn ridge_occludes_target_behind_it() {
        // Observer sits well inside the tile (not on a degree boundary — see note below);
        // ridge and target lie due north of it (same longitude), so distance-from-observer
        // is just `(lat - BASE_LAT)` in metres.
        const BASE_LAT: f64 = 46.05;
        let dem_with_ridge = synthetic_dem(|lat, _lon| {
            let d_m = (lat - BASE_LAT) * 111_320.0;
            if (1_800.0..2_300.0).contains(&d_m) {
                200.0 // a ridge ~2 km out
            } else {
                0.0
            }
        });

        // Kept off exact degree boundaries: floating-point rounding in
        // `great_circle_point`'s first sample can otherwise nudge it into the
        // neighbouring tile, which this single-tile synthetic Dem doesn't have.
        let observer = Geodetic::new(BASE_LAT, -121.5, 2.0); // 2 m eye height, flat ground
        let target = Geodetic::new(BASE_LAT + 5_000.0 / 111_320.0, -121.5, 50.0); // low hill, 5 km out

        let occluded = check(&dem_with_ridge, observer, target, VisibilityConfig::default());
        assert!(
            !occluded.is_visible(),
            "expected occlusion by the ridge, got {occluded:?}"
        );

        let dem_flat = synthetic_dem(|_, _| 0.0);
        let visible = check(&dem_flat, observer, target, VisibilityConfig::default());
        assert!(
            visible.is_visible(),
            "expected visibility with the ridge removed, got {visible:?}"
        );
    }

    /// Build a [`Dem`] backed by a single in-memory tile (N46/W122) so tests don't touch
    /// the filesystem or network.
    fn synthetic_dem(f: impl Fn(f64, f64) -> f32) -> Dem {
        use crate::dem::TILE_DIM;

        let mut grid = vec![0f32; (TILE_DIM * TILE_DIM) as usize];
        for row in 0..TILE_DIM {
            for col in 0..TILE_DIM {
                let lat_here = 47.0 - (row as f64) / TILE_DIM as f64;
                let lon_here = -122.0 + (col as f64) / TILE_DIM as f64;
                grid[(row * TILE_DIM + col) as usize] = f(lat_here, lon_here);
            }
        }
        Dem::from_tiles_for_test(vec![((46, -122), grid)])
    }
}
