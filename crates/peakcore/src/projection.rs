//! Camera pose, projection, and label layout (M4).

use serde::{Deserialize, Serialize};
use specta::Type;

/// Real capture intrinsics, as reported by the device, for a preview rendered with
/// `resizeAspectFill` into the app's container.
///
/// Deriving the focal length from a single assumed on-screen horizontal FOV (what
/// [`CameraPose::hfov_deg`] does) is wrong on a phone for three compounding reasons, so
/// this carries the raw quantities and does the conversion in one place instead:
///
/// 1. `fov_deg` is measured across the capture buffer's *long* axis, which maps to the
///    screen's long axis — height in portrait, width in landscape.
/// 2. `resizeAspectFill` scales the buffer to *cover* the container and crops the
///    overflow, so one of the two on-screen FOVs is always narrower than `fov_deg`.
/// 3. Zoom crops further still, tightening the FOV by `zoom_factor`.
///
/// For a 1920x1080 buffer at `fov_deg` 68 on a 393x852 portrait screen, the three
/// together put the real on-screen horizontal FOV near 35 deg — about half the 63 deg
/// that was assumed before this existed. Turn the same phone to landscape and the crop
/// moves to the other axis: the horizontal FOV becomes the full 68 deg and the vertical
/// one is what narrows.
///
/// Orientation-independent, and only because the capture chain is. The plugin rotates
/// the preview layer, the fitter's frames and the still photo with the interface (see
/// `applyCaptureOrientation` in `CameraPlugin.swift`), so the buffer's long axis is
/// always the screen's long axis and there is one aspect-fill rule rather than one per
/// orientation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CameraIntrinsics {
    /// Native FOV across the buffer's long axis, degrees, at zoom 1.0.
    pub fov_deg: f64,
    pub zoom_factor: f64,
    /// Capture buffer dimensions in the sensor's native landscape orientation.
    pub buffer_long_px: f64,
    pub buffer_short_px: f64,
}

impl CameraIntrinsics {
    /// Focal length in *screen* pixels, following the buffer -> zoom -> aspect-fill chain.
    ///
    /// Pixels are square, so one focal length covers both screen axes; the caller's
    /// existing `x = w/2 + f * dot(v,r)/z` projection then gets both FOVs right for free.
    pub fn focal_px(&self, screen_w: u32, screen_h: u32) -> f64 {
        // Focal length in buffer pixels, then tightened by the zoom crop.
        let f_buf =
            (self.buffer_long_px / 2.0) / (self.fov_deg.to_radians() / 2.0).tan() * self.zoom_factor;
        // The buffer presents with its long axis along the screen's long axis, whichever
        // that currently is: portrait puts it on height, landscape on width. Pairing the
        // axes by length rather than by name is what makes this hold in both, and it is
        // only true because the capture connection is rotated with the interface. `max`
        // is what makes this aspect *fill* (cover and crop) rather than fit.
        let screen_long = f64::from(screen_w.max(screen_h));
        let screen_short = f64::from(screen_w.min(screen_h));
        let cover = (screen_long / self.buffer_long_px).max(screen_short / self.buffer_short_px);
        f_buf * cover
    }

    /// Focal length in *capture frame* pixels, for a frame scaled to `frame_long_px` on
    /// its long axis.
    ///
    /// Distinct from [`focal_px`](Self::focal_px), and the two must not be confused. That
    /// one describes the image after `resizeAspectFill` has cropped it to the screen, and
    /// is what the on-screen overlay needs. This one describes the *uncropped* capture
    /// buffer, and is what [`crate::skyline::fit`] needs — fitting against the raw frame
    /// avoids modelling the crop at all, and keeps the sensor's full field of view, which
    /// on a portrait phone is exactly the axis the display throws away.
    ///
    /// Pose corrections derived through this focal length still apply unchanged to the
    /// display pose: yaw and pitch offsets describe where the camera points, not how its
    /// image is later cropped.
    pub fn frame_focal_px(&self, frame_long_px: f64) -> f64 {
        let scale = frame_long_px / self.buffer_long_px;
        (self.buffer_long_px / 2.0) / (self.fov_deg.to_radians() / 2.0).tan()
            * self.zoom_factor
            * scale
    }
}

/// Camera orientation and intrinsics. `yaw`/`pitch`/`roll` describe where the camera is
/// pointed in the observer's local ENU frame; the focal length comes from `intrinsics`
/// when the device reported them, else from `hfov` + `width`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CameraPose {
    /// True-north azimuth, degrees, clockwise.
    pub yaw_deg: f64,
    /// Degrees, up positive.
    pub pitch_deg: f64,
    /// Degrees, clockwise looking along the forward axis. Zero unless you have a real
    /// gravity-referenced pose (ARKit/ARCore) to drive it.
    pub roll_deg: f64,
    /// Assumed on-screen horizontal FOV. Only used when `intrinsics` is `None` — a
    /// fallback for callers with no device to ask (peaklab's manual `--hfov`) and for
    /// the first few ticks before the first intrinsics reading arrives.
    pub hfov_deg: f64,
    pub width: u32,
    pub height: u32,
    /// Real device intrinsics, when available. Takes precedence over `hfov_deg`.
    #[serde(default)]
    pub intrinsics: Option<CameraIntrinsics>,
}

/// Vector helpers. `[E, N, U]` throughout, matching [`crate::geo::enu`].
fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn norm(a: [f64; 3]) -> [f64; 3] {
    let n = dot(a, a).sqrt();
    [a[0] / n, a[1] / n, a[2] / n]
}
/// Rodrigues' rotation formula: rotate `v` by `angle_rad` about unit axis `k`.
fn rotate_about(v: [f64; 3], k: [f64; 3], angle_rad: f64) -> [f64; 3] {
    let (s, c) = angle_rad.sin_cos();
    let kxv = cross(k, v);
    let kdv = dot(k, v);
    [
        v[0] * c + kxv[0] * s + k[0] * kdv * (1.0 - c),
        v[1] * c + kxv[1] * s + k[1] * kdv * (1.0 - c),
        v[2] * c + kxv[2] * s + k[2] * kdv * (1.0 - c),
    ]
}

impl CameraPose {
    /// Forward/right/up basis vectors in ENU, roll applied about the forward axis.
    pub fn basis(&self) -> ([f64; 3], [f64; 3], [f64; 3]) {
        let yaw = self.yaw_deg.to_radians();
        let pitch = self.pitch_deg.to_radians();

        let forward = [
            yaw.sin() * pitch.cos(),
            yaw.cos() * pitch.cos(),
            pitch.sin(),
        ];
        let right0 = [yaw.cos(), -yaw.sin(), 0.0];
        let up0 = norm(cross(right0, forward));

        if self.roll_deg == 0.0 {
            return (forward, right0, up0);
        }
        let roll = self.roll_deg.to_radians();
        (
            forward,
            rotate_about(right0, forward, roll),
            rotate_about(up0, forward, roll),
        )
    }

    pub fn focal_px(&self) -> f64 {
        match self.intrinsics {
            Some(i) => i.focal_px(self.width, self.height),
            None => (self.width as f64 / 2.0) / (self.hfov_deg.to_radians() / 2.0).tan(),
        }
    }

    /// Horizontal FOV actually spanned by the image on screen. Equals `hfov_deg` without
    /// intrinsics; with them it's the post-crop, post-zoom value, which is what the debug
    /// HUD wants to show.
    pub fn effective_hfov_deg(&self) -> f64 {
        2.0 * (self.width as f64 / 2.0 / self.focal_px()).atan().to_degrees()
    }

    /// Project an ENU vector to pixel coordinates (origin top-left, y down).
    /// `None` if the point is behind the camera.
    ///
    /// Recomputes [`basis`](Self::basis) on every call, which is fine for projecting a
    /// handful of points against a handful of camera poses (peaklab's `render`
    /// subcommand). A caller projecting many points against one fixed pose in a tight
    /// loop — the AR view's per-tick projection — should call [`basis`](Self::basis)
    /// once and use [`project_with_basis`] instead.
    pub fn project(&self, target_enu: [f64; 3]) -> Option<(f64, f64)> {
        project_with_basis(target_enu, self.basis(), self.focal_px(), self.width, self.height)
    }

    /// Vertical FOV implied by the focal length and the image height.
    pub fn vfov_deg(&self) -> f64 {
        let f_px = self.focal_px();
        2.0 * (self.height as f64 / 2.0 / f_px).atan().to_degrees()
    }
}

/// Project an ENU vector to pixel coordinates using an already-computed camera basis
/// (from [`CameraPose::basis`]) instead of recomputing it. `None` if the point is
/// behind the camera.
///
/// Projecting many peaks against one fixed pose and calling `basis()` fresh each time —
/// as [`CameraPose::project`] does — recomputes the same yaw/pitch trig and cross
/// products for every point. Hoisting `basis()` out of the loop and calling this
/// instead turns that into eight trig calls total per tick rather than eight per point.
pub fn project_with_basis(
    target_enu: [f64; 3],
    basis: ([f64; 3], [f64; 3], [f64; 3]),
    focal_px: f64,
    width: u32,
    height: u32,
) -> Option<(f64, f64)> {
    let (f, r, u) = basis;
    let z = dot(target_enu, f);
    if z <= 0.0 {
        return None;
    }
    let x = width as f64 / 2.0 + focal_px * dot(target_enu, r) / z;
    let y = height as f64 / 2.0 - focal_px * dot(target_enu, u) / z;
    Some((x, y))
}

/// Project a `(azimuth_deg, elevation_deg)` horizon sweep into drawable polylines.
///
/// A sweep is a ring of samples, and turning it into something a renderer can stroke
/// means answering three questions the raw projected points cannot: which neighbouring
/// samples are actually adjacent in azimuth (a sweep omits azimuths with no DEM coverage,
/// so consecutive entries can straddle a hole), which points are in front of the camera
/// at all, and which of the survivors are near enough the viewport to be worth sending.
///
/// All three are answered here, in the frame where the answers exist, rather than in the
/// renderer. The overlay used to receive every front-hemisphere sample — around half of a
/// 720-point sweep, with coordinates running to ±10⁶ px at the edges where the ray is
/// nearly perpendicular to the view — and re-derive the breaks by splitting wherever two
/// consecutive points landed further apart on screen than a viewport diagonal. That
/// heuristic cannot tell a genuine coverage hole from the projection blowing up near
/// ±90°, and it read the wrap-around and the far-off-screen tail as breaks too.
///
/// `azimuth_step_deg` is the sweep's own spacing; samples further apart than 1.5× it are
/// treated as separated by a hole. `margin_px` keeps points that far outside the viewport,
/// so a polyline still runs off the edge of the frame rather than stopping short of it.
pub fn project_horizon(
    horizon: &[(f64, f64)],
    azimuth_step_deg: f64,
    basis: ([f64; 3], [f64; 3], [f64; 3]),
    focal_px: f64,
    width: u32,
    height: u32,
    margin_px: f64,
) -> Vec<Vec<(f64, f64)>> {
    /// Nominal range for turning a look angle back into a direction vector. Scale is
    /// irrelevant to a pinhole projection; only the direction matters.
    const RANGE_M: f64 = 50_000.0;

    let n = horizon.len();
    if n < 2 {
        return Vec::new();
    }

    let projected: Vec<Option<(f64, f64)>> = horizon
        .iter()
        .map(|&(az, el)| {
            let v = crate::geo::enu_from_look_angles(az, el, RANGE_M);
            project_with_basis(v, basis, focal_px, width, height)
                .filter(|(x, y)| x.is_finite() && y.is_finite())
        })
        .collect();

    let near = |(x, y): (f64, f64)| {
        x >= -margin_px
            && x <= width as f64 + margin_px
            && y >= -margin_px
            && y <= height as f64 + margin_px
    };

    let mut segments: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut current: Vec<(f64, f64)> = Vec::new();

    // Pairs walk the ring, so the last pair closes 359.5° back onto 0°. A sweep that
    // doesn't reach all the way round simply fails the adjacency test there.
    for i in 0..n {
        let j = (i + 1) % n;
        let adjacent =
            crate::geo::angle_diff_deg(horizon[j].0, horizon[i].0).abs() <= azimuth_step_deg * 1.5;
        let drawable = match (adjacent, projected[i], projected[j]) {
            // One endpoint on (or near) screen is enough: that keeps the edge that
            // crosses the frame boundary, so the line reaches it.
            (true, Some(a), Some(b)) => near(a) || near(b),
            _ => false,
        };

        if drawable {
            let (a, b) = (projected[i].expect("drawable"), projected[j].expect("drawable"));
            if current.is_empty() {
                current.push(a);
            }
            current.push(b);
        } else if !current.is_empty() {
            segments.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }

    segments
}

/// A rectangle in pixel space, used for label-overlap testing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    fn overlaps(&self, other: &Rect) -> bool {
        self.x < other.x + other.w
            && self.x + self.w > other.x
            && self.y < other.y + other.h
            && self.y + self.h > other.y
    }
}

/// A label placed (or not) for one peak.
pub struct PlacedLabel {
    pub name: String,
    /// Projected pixel position of the peak itself.
    pub anchor: (f64, f64),
    /// Text bounding box, if a non-overlapping spot was found.
    pub text_rect: Option<Rect>,
}

/// Greedily place labels nearest-first, skipping any position that overlaps an
/// already-placed label's box. Tries a short vertical stack above the anchor before
/// giving up — the plan's suggested "leader line" is what makes a stacked label still
/// legible: without one there'd be no visual link back to a marker offset this far away.
///
/// Candidates must already be sorted nearest-first (closest peaks claim their preferred
/// position); `measure` returns a text's `(width, height)` in pixels for a given string.
pub fn layout_labels(
    candidates: &[(String, (f64, f64))],
    measure: impl Fn(&str) -> (f64, f64),
    max_stack: usize,
    line_gap: f64,
) -> Vec<PlacedLabel> {
    let mut placed_rects: Vec<Rect> = Vec::new();
    let mut out = Vec::with_capacity(candidates.len());

    for (name, anchor) in candidates {
        let (tw, th) = measure(name);
        let mut chosen = None;

        for stack in 0..=max_stack {
            // Centred above the anchor, stacking upward on collision.
            let rect = Rect {
                x: anchor.0 - tw / 2.0,
                y: anchor.1 - 10.0 - (th + line_gap) * (stack as f64 + 1.0),
                w: tw,
                h: th,
            };
            if !placed_rects.iter().any(|p| p.overlaps(&rect)) {
                chosen = Some(rect);
                break;
            }
        }

        if let Some(rect) = chosen {
            placed_rects.push(rect);
        }
        out.push(PlacedLabel {
            name: name.clone(),
            anchor: *anchor,
            text_rect: chosen,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn straight_ahead_projects_to_image_center() {
        let cam = CameraPose {
            yaw_deg: 90.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
            hfov_deg: 60.0,
            width: 1000,
            height: 800,
            intrinsics: None,
        };
        // Due east, same altitude: exactly along the forward axis at yaw=90.
        let (x, y) = cam.project([100.0, 0.0, 0.0]).unwrap();
        assert!((x - 500.0).abs() < 1e-6);
        assert!((y - 400.0).abs() < 1e-6);
    }

    #[test]
    fn behind_camera_is_none() {
        let cam = CameraPose {
            yaw_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
            hfov_deg: 60.0,
            width: 1000,
            height: 800,
            intrinsics: None,
        };
        assert!(cam.project([0.0, -100.0, 0.0]).is_none());
    }

    #[test]
    fn point_right_of_center_projects_to_positive_x() {
        let cam = CameraPose {
            yaw_deg: 0.0, // facing north
            pitch_deg: 0.0,
            roll_deg: 0.0,
            hfov_deg: 90.0,
            width: 1000,
            height: 1000,
            intrinsics: None,
        };
        // East-of-north and slightly north: right of center, screen x > 500.
        let (x, _) = cam.project([50.0, 100.0, 0.0]).unwrap();
        assert!(x > 500.0, "expected right-of-center, got x={x}");
    }

    /// A stand-in for an iPhone held portrait: 1920x1080 capture, ~68 deg native FOV.
    fn iphone_pose(zoom: f64) -> CameraPose {
        CameraPose {
            yaw_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
            hfov_deg: 63.0, // the old guess; must be ignored once intrinsics are present
            width: 393,
            height: 852,
            intrinsics: Some(CameraIntrinsics {
                fov_deg: 68.0,
                zoom_factor: zoom,
                buffer_long_px: 1920.0,
                buffer_short_px: 1080.0,
            }),
        }
    }

    #[test]
    fn aspect_fill_crop_narrows_horizontal_fov_far_below_the_native_value() {
        let cam = iphone_pose(1.0);
        // Held portrait under resizeAspectFill the long axis fits the screen height, so
        // the native 68 deg lands on the *vertical* axis and the horizontal is cropped
        // down to ~35 deg — not the 63 deg the pre-intrinsics code assumed.
        assert!(
            (cam.vfov_deg() - 68.0).abs() < 0.5,
            "vfov should be the native long-axis FOV, got {}",
            cam.vfov_deg()
        );
        assert!(
            (cam.effective_hfov_deg() - 34.6).abs() < 0.5,
            "hfov should be cropped to ~34.6 deg, got {}",
            cam.effective_hfov_deg()
        );
    }

    /// The same phone turned on its side. Nothing about the capture changes — the buffer
    /// is still 1920x1080 at 68 deg — but the aspect-fill crop moves to the other axis,
    /// so the FOVs swap rather than staying where portrait left them.
    fn iphone_pose_landscape(zoom: f64) -> CameraPose {
        CameraPose {
            width: 852,
            height: 393,
            ..iphone_pose(zoom)
        }
    }

    #[test]
    fn landscape_puts_the_native_fov_on_the_horizontal_axis() {
        let cam = iphone_pose_landscape(1.0);
        assert!(
            (cam.effective_hfov_deg() - 68.0).abs() < 0.5,
            "hfov should be the native long-axis FOV in landscape, got {}",
            cam.effective_hfov_deg()
        );
        assert!(
            (cam.vfov_deg() - 34.6).abs() < 0.5,
            "vfov should be the cropped axis in landscape, got {}",
            cam.vfov_deg()
        );
    }

    /// Rotating the phone must not change the focal length: the same lens is imaging the
    /// same buffer onto the same number of screen pixels per degree, only about a
    /// different axis. Reading the portrait rule in landscape scaled it by ~0.79 instead
    /// of ~0.44 — a 78% overestimate, which lands as peaks sliding off the edge of the
    /// frame the moment the phone is turned.
    #[test]
    fn focal_length_is_the_same_in_both_orientations() {
        let portrait = iphone_pose(1.0).focal_px();
        let landscape = iphone_pose_landscape(1.0).focal_px();
        assert!(
            (portrait - landscape).abs() < 1e-9,
            "focal length changed with orientation: {portrait} vs {landscape}"
        );
    }

    #[test]
    fn zoom_tightens_the_field_of_view() {
        // Doubling zoom doubles the focal length, which halves tan(fov/2).
        let half_tan_1x = (iphone_pose(1.0).effective_hfov_deg().to_radians() / 2.0).tan();
        let half_tan_2x = (iphone_pose(2.0).effective_hfov_deg().to_radians() / 2.0).tan();
        assert!(
            (half_tan_1x / half_tan_2x - 2.0).abs() < 1e-9,
            "2x zoom should halve tan(hfov/2): {half_tan_1x} vs {half_tan_2x}"
        );
    }

    #[test]
    fn without_intrinsics_focal_length_still_comes_from_hfov() {
        // peaklab and the pre-first-reading ticks depend on this path being untouched.
        let cam = CameraPose {
            yaw_deg: 0.0,
            pitch_deg: 0.0,
            roll_deg: 0.0,
            hfov_deg: 66.0,
            width: 1000,
            height: 800,
            intrinsics: None,
        };
        let expected = 500.0 / (66.0f64.to_radians() / 2.0).tan();
        assert!((cam.focal_px() - expected).abs() < 1e-9);
        assert!((cam.effective_hfov_deg() - 66.0).abs() < 1e-9);
    }

    /// A full 360 deg sweep at 0.5 deg, flat except for a bump, matching what the app
    /// hands `project_horizon`.
    fn full_sweep() -> Vec<(f64, f64)> {
        (0..720)
            .map(|i| {
                let az = f64::from(i) * 0.5;
                (az, 5.0 * (az.to_radians() * 3.0).sin())
            })
            .collect()
    }

    fn horizon_pose() -> CameraPose {
        CameraPose {
            yaw_deg: 203.0,
            pitch_deg: -4.0,
            roll_deg: 0.0,
            hfov_deg: 63.0,
            width: 402,
            height: 874,
            intrinsics: None,
        }
    }

    #[test]
    fn horizon_is_culled_to_the_viewport_and_stays_continuous_across_it() {
        let pose = horizon_pose();
        let segments = project_horizon(
            &full_sweep(),
            0.5,
            pose.basis(),
            pose.focal_px(),
            pose.width,
            pose.height,
            200.0,
        );

        // The near-perpendicular rays that used to reach +/-10^6 px are gone: everything
        // returned is within one point of the margin box.
        let points: Vec<(f64, f64)> = segments.iter().flatten().copied().collect();
        assert!(!points.is_empty(), "expected a horizon in front of the camera");
        for &(x, y) in &points {
            assert!(
                x.abs() < 100_000.0 && y.abs() < 100_000.0,
                "runaway coordinate ({x}, {y})"
            );
        }

        // Half a degree of azimuth is a few pixels at this focal length, so a correctly
        // assembled polyline has no large jumps inside a segment.
        for seg in &segments {
            for pair in seg.windows(2) {
                let d = (pair[1].0 - pair[0].0).hypot(pair[1].1 - pair[0].1);
                assert!(d < 200.0, "gap of {d}px inside a segment: {pair:?}");
            }
        }

        // And the line still spans the frame rather than stopping at its edges.
        let (min_x, max_x) = points.iter().fold((f64::MAX, f64::MIN), |(lo, hi), &(x, _)| {
            (lo.min(x), hi.max(x))
        });
        assert!(min_x <= 0.0, "left edge not reached: {min_x}");
        assert!(max_x >= f64::from(pose.width), "right edge not reached: {max_x}");
    }

    #[test]
    fn horizon_breaks_where_the_sweep_has_a_coverage_hole() {
        // `sweep_horizon` omits azimuths with no DEM coverage rather than zero-filling
        // them, so consecutive entries can straddle a hole. Drawing across one would put
        // a chord where there is no terrain data at all.
        let sweep: Vec<(f64, f64)> = full_sweep()
            .into_iter()
            .filter(|&(az, _)| !(200.0..210.0).contains(&az))
            .collect();
        let pose = horizon_pose();
        let segments = project_horizon(
            &sweep,
            0.5,
            pose.basis(),
            pose.focal_px(),
            pose.width,
            pose.height,
            200.0,
        );
        assert!(
            segments.len() >= 2,
            "expected the hole to split the line, got {} segment(s)",
            segments.len()
        );
    }

    #[test]
    fn horizon_facing_north_does_not_draw_across_the_wraparound() {
        // Looking at 0 deg, the visible arc runs 315..360 and 0..45 — adjacent in azimuth
        // but at opposite ends of the array. Joining them naively sweeps a line across the
        // whole frame; dropping the pair leaves a seam the renderer never notices.
        let pose = CameraPose {
            yaw_deg: 0.0,
            ..horizon_pose()
        };
        let segments = project_horizon(
            &full_sweep(),
            0.5,
            pose.basis(),
            pose.focal_px(),
            pose.width,
            pose.height,
            200.0,
        );
        for seg in &segments {
            for pair in seg.windows(2) {
                let d = (pair[1].0 - pair[0].0).hypot(pair[1].1 - pair[0].1);
                assert!(d < 200.0, "wraparound drew a {d}px chord: {pair:?}");
            }
        }
        let n: usize = segments.iter().map(Vec::len).sum();
        assert!(n > 100, "expected the arc either side of north, got {n} points");
    }

    #[test]
    fn overlapping_labels_stack_instead_of_colliding() {
        let candidates = vec![
            ("A".to_string(), (500.0, 500.0)),
            ("B".to_string(), (505.0, 500.0)), // nearly identical anchor -> forces a stack
        ];
        let placed = layout_labels(&candidates, |_| (60.0, 20.0), 5, 4.0);
        let ra = placed[0].text_rect.unwrap();
        let rb = placed[1].text_rect.unwrap();
        assert!(!ra.overlaps(&rb), "labels should not overlap: {ra:?} vs {rb:?}");
    }
}
