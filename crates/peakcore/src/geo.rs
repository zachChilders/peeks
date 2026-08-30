//! WGS84 geodesy: ECEF/ENU transforms, look angles, great-circle helpers.
//!
//! Everything downstream (visibility, projection) works in the observer's local ENU
//! frame. Going through ECEF means Earth curvature is handled exactly rather than by
//! bolting a correction term onto flat-earth trig.

use serde::{Deserialize, Serialize};
use specta::Type;

pub const WGS84_A: f64 = 6_378_137.0;
pub const WGS84_F: f64 = 1.0 / 298.257_223_563;
pub const WGS84_E2: f64 = WGS84_F * (2.0 - WGS84_F);

/// Mean Earth radius, used only for great-circle distance and the refraction term.
pub const EARTH_MEAN_R: f64 = 6_371_008.8;

/// Effective-radius coefficient for standard atmospheric refraction.
pub const REFRACTION_K: f64 = 7.0 / 6.0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Type)]
pub struct Geodetic {
    /// Degrees, north positive.
    pub lat: f64,
    /// Degrees, east positive.
    pub lon: f64,
    /// Metres above the ellipsoid (see the geoid caveat in README).
    pub alt: f64,
}

impl Geodetic {
    pub fn new(lat: f64, lon: f64, alt: f64) -> Self {
        Self { lat, lon, alt }
    }

    pub fn to_ecef(self) -> [f64; 3] {
        let (sin_lat, cos_lat) = self.lat.to_radians().sin_cos();
        let (sin_lon, cos_lon) = self.lon.to_radians().sin_cos();
        let n = WGS84_A / (1.0 - WGS84_E2 * sin_lat * sin_lat).sqrt();
        [
            (n + self.alt) * cos_lat * cos_lon,
            (n + self.alt) * cos_lat * sin_lon,
            (n * (1.0 - WGS84_E2) + self.alt) * sin_lat,
        ]
    }
}

/// Vector from `observer` to `target` in the observer's local East/North/Up frame.
pub fn enu(observer: Geodetic, target: Geodetic) -> [f64; 3] {
    let o = observer.to_ecef();
    let t = target.to_ecef();
    let d = [t[0] - o[0], t[1] - o[1], t[2] - o[2]];

    let (sin_lat, cos_lat) = observer.lat.to_radians().sin_cos();
    let (sin_lon, cos_lon) = observer.lon.to_radians().sin_cos();

    [
        -sin_lon * d[0] + cos_lon * d[1],
        -sin_lat * cos_lon * d[0] - sin_lat * sin_lon * d[1] + cos_lat * d[2],
        cos_lat * cos_lon * d[0] + cos_lat * sin_lon * d[1] + sin_lat * d[2],
    ]
}

/// Azimuth of an ENU vector: degrees clockwise from true north, in `[0, 360)`.
pub fn azimuth_deg(v: [f64; 3]) -> f64 {
    let a = v[0].atan2(v[1]).to_degrees();
    if a < 0.0 {
        a + 360.0
    } else {
        a
    }
}

/// Geometric elevation angle of an ENU vector, in degrees (up positive).
pub fn elevation_deg(v: [f64; 3]) -> f64 {
    v[2].atan2(v[0].hypot(v[1])).to_degrees()
}

pub fn horizontal_range(v: [f64; 3]) -> f64 {
    v[0].hypot(v[1])
}

/// Apparent upward lift from standard atmospheric refraction, in degrees.
///
/// Refraction is conventionally folded in by replacing R with `k*R`, which shrinks the
/// curvature drop over distance `d` from `d²/2R` to `d²/2kR`. Converting that height
/// difference back to an angle at range `d` gives `d(1 - 1/k) / 2R`.
///
/// Applied on top of the exact ECEF elevation angle, so the geometry stays consistent
/// between targets and the terrain samples that might occlude them.
pub fn refraction_lift_deg(distance_m: f64) -> f64 {
    (distance_m * (1.0 - 1.0 / REFRACTION_K) / (2.0 * EARTH_MEAN_R)).to_degrees()
}

/// Look angles from observer to target: `(azimuth_deg, apparent_elevation_deg, range_m)`.
///
/// The elevation angle includes the refraction correction; `range_m` is the straight-line
/// (slant) distance.
pub fn look_angles(observer: Geodetic, target: Geodetic) -> (f64, f64, f64) {
    let v = enu(observer, target);
    let range = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    let elev = elevation_deg(v) + refraction_lift_deg(horizontal_range(v));
    (azimuth_deg(v), elev, range)
}

/// Great-circle (surface) distance in metres.
pub fn great_circle_distance(a: Geodetic, b: Geodetic) -> f64 {
    let (lat1, lat2) = (a.lat.to_radians(), b.lat.to_radians());
    let dlat = lat2 - lat1;
    let dlon = (b.lon - a.lon).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_MEAN_R * h.sqrt().asin()
}

/// Point at `frac` along the great circle from `a` to `b`, as `(lat, lon)` degrees.
///
/// Spherical interpolation: good to a few metres against the ellipsoidal geodesic at
/// these ranges, which is far below the DEM posting.
pub fn great_circle_point(a: Geodetic, b: Geodetic, frac: f64) -> (f64, f64) {
    let (lat1, lon1) = (a.lat.to_radians(), a.lon.to_radians());
    let (lat2, lon2) = (b.lat.to_radians(), b.lon.to_radians());

    let d = great_circle_distance(a, b) / EARTH_MEAN_R;
    if d.abs() < 1e-12 {
        return (a.lat, a.lon);
    }

    let (sa, sb) = (
        ((1.0 - frac) * d).sin() / d.sin(),
        (frac * d).sin() / d.sin(),
    );

    let x = sa * lat1.cos() * lon1.cos() + sb * lat2.cos() * lon2.cos();
    let y = sa * lat1.cos() * lon1.sin() + sb * lat2.cos() * lon2.sin();
    let z = sa * lat1.sin() + sb * lat2.sin();

    (
        z.atan2(x.hypot(y)).to_degrees(),
        y.atan2(x).to_degrees(),
    )
}

/// Point at `distance_m` along `azimuth_deg` (true north, clockwise) from `origin`, as
/// `(lat, lon)` degrees. The inverse-direction counterpart of [`great_circle_point`]:
/// that walks between two known endpoints, this walks outward along a bearing with no
/// endpoint — used to sweep a horizon profile across azimuths rather than along one
/// specific path to a target. Same spherical-approximation trade-off.
pub fn destination_point(origin: Geodetic, azimuth_deg: f64, distance_m: f64) -> (f64, f64) {
    let ang_dist = distance_m / EARTH_MEAN_R;
    let brng = azimuth_deg.to_radians();
    let lat1 = origin.lat.to_radians();
    let lon1 = origin.lon.to_radians();

    let lat2 = (lat1.sin() * ang_dist.cos() + lat1.cos() * ang_dist.sin() * brng.cos()).asin();
    let lon2 =
        lon1 + (brng.sin() * ang_dist.sin() * lat1.cos()).atan2(ang_dist.cos() - lat1.sin() * lat2.sin());

    (lat2.to_degrees(), lon2.to_degrees())
}

/// ENU vector matching the given azimuth/elevation angles at `range_m` from the
/// observer. The inverse of [`azimuth_deg`]/[`elevation_deg`] taken together: those turn
/// a vector into angles, this turns angles back into a vector — for projecting a horizon
/// line (which only has an angle, not a real target point; see
/// `crate::visibility::horizon_at_azimuth`) through the same camera-projection path peak
/// dots use. `range_m` only sets the vector's scale, not its direction, so any positive
/// value projects identically through a pinhole camera model.
pub fn enu_from_look_angles(azimuth_deg: f64, elevation_deg: f64, range_m: f64) -> [f64; 3] {
    let az = azimuth_deg.to_radians();
    let el = elevation_deg.to_radians();
    let horiz = range_m * el.cos();
    [horiz * az.sin(), horiz * az.cos(), range_m * el.sin()]
}

/// Smallest signed difference `a - b` between two azimuths, in `(-180, 180]`.
pub fn angle_diff_deg(a: f64, b: f64) -> f64 {
    let mut d = (a - b) % 360.0;
    if d > 180.0 {
        d -= 360.0;
    } else if d <= -180.0 {
        d += 360.0;
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecef_roundtrip_origin() {
        let p = Geodetic::new(0.0, 0.0, 0.0);
        let e = p.to_ecef();
        assert!((e[0] - WGS84_A).abs() < 1e-6);
        assert!(e[1].abs() < 1e-6 && e[2].abs() < 1e-6);
    }

    #[test]
    fn due_north_and_east_azimuths() {
        let o = Geodetic::new(46.0, -121.0, 0.0);
        let n = Geodetic::new(46.5, -121.0, 0.0);
        let e = Geodetic::new(46.0, -120.5, 0.0);
        // Compared with wrapping: due north can land on either side of 0/360.
        assert!(angle_diff_deg(azimuth_deg(enu(o, n)), 0.0).abs() < 1e-6);
        assert!(angle_diff_deg(azimuth_deg(enu(o, e)), 90.0).abs() < 0.3);
    }

    #[test]
    fn straight_up_is_ninety_degrees() {
        let o = Geodetic::new(46.0, -121.0, 0.0);
        let up = Geodetic::new(46.0, -121.0, 1000.0);
        assert!((elevation_deg(enu(o, up)) - 90.0).abs() < 1e-6);
    }

    #[test]
    fn sea_level_target_dips_below_horizon() {
        // 60 km away at the same altitude should sit below the horizontal by roughly
        // the curvature drop, ~0.27° geometric before refraction.
        let o = Geodetic::new(46.0, -121.0, 0.0);
        let t = Geodetic::new(46.0 + 60_000.0 / 111_320.0, -121.0, 0.0);
        let geometric = elevation_deg(enu(o, t));
        assert!(geometric < 0.0, "expected dip, got {geometric}");
        assert!((geometric.abs() - 0.27).abs() < 0.05, "got {geometric}");
    }

    #[test]
    fn midpoint_interpolation_is_halfway() {
        let a = Geodetic::new(46.0, -121.0, 0.0);
        let b = Geodetic::new(47.0, -121.0, 0.0);
        let (lat, lon) = great_circle_point(a, b, 0.5);
        assert!((lat - 46.5).abs() < 1e-6, "lat {lat}");
        assert!((lon + 121.0).abs() < 1e-6, "lon {lon}");
    }

    #[test]
    fn destination_point_matches_due_north_offset() {
        let o = Geodetic::new(46.0, -121.0, 0.0);
        let (lat, lon) = destination_point(o, 0.0, 10_000.0);
        // Due north: longitude unchanged, latitude advances by distance/EARTH_MEAN_R.
        assert!((lon + 121.0).abs() < 1e-6, "lon {lon}");
        assert!(
            (lat - (46.0 + (10_000.0 / EARTH_MEAN_R).to_degrees())).abs() < 1e-6,
            "lat {lat}"
        );
    }

    #[test]
    fn enu_from_look_angles_round_trips_through_azimuth_and_elevation() {
        for (az, el) in [(0.0, 0.0), (37.0, 12.0), (180.0, -5.0), (270.0, 45.0)] {
            let v = enu_from_look_angles(az, el, 1_000.0);
            assert!((angle_diff_deg(azimuth_deg(v), az)).abs() < 1e-9, "az {az} -> {v:?}");
            assert!((elevation_deg(v) - el).abs() < 1e-9, "el {el} -> {v:?}");
        }
    }

    #[test]
    fn destination_point_round_trips_with_great_circle_distance() {
        let o = Geodetic::new(46.5, -121.3, 0.0);
        let (lat, lon) = destination_point(o, 217.0, 15_000.0);
        let dist = great_circle_distance(o, Geodetic::new(lat, lon, 0.0));
        assert!((dist - 15_000.0).abs() < 1.0, "got {dist}");
    }

    #[test]
    fn azimuth_wrap() {
        assert!((angle_diff_deg(1.0, 359.0) - 2.0).abs() < 1e-9);
        assert!((angle_diff_deg(359.0, 1.0) + 2.0).abs() < 1e-9);
    }
}
