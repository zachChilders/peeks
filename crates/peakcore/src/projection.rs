//! Camera pose, projection, and label layout (M4).

/// Camera orientation and intrinsics. `yaw`/`pitch`/`roll` describe where the camera is
/// pointed in the observer's local ENU frame; `hfov` + `width` derive the focal length.
#[derive(Debug, Clone, Copy)]
pub struct CameraPose {
    /// True-north azimuth, degrees, clockwise.
    pub yaw_deg: f64,
    /// Degrees, up positive.
    pub pitch_deg: f64,
    /// Degrees, clockwise looking along the forward axis. Zero unless you have a real
    /// gravity-referenced pose (ARKit/ARCore) to drive it.
    pub roll_deg: f64,
    pub hfov_deg: f64,
    pub width: u32,
    pub height: u32,
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
        (self.width as f64 / 2.0) / (self.hfov_deg.to_radians() / 2.0).tan()
    }

    /// Project an ENU vector to pixel coordinates (origin top-left, y down).
    /// `None` if the point is behind the camera.
    pub fn project(&self, target_enu: [f64; 3]) -> Option<(f64, f64)> {
        let (f, r, u) = self.basis();
        let z = dot(target_enu, f);
        if z <= 0.0 {
            return None;
        }
        let f_px = self.focal_px();
        let x = self.width as f64 / 2.0 + f_px * dot(target_enu, r) / z;
        let y = self.height as f64 / 2.0 - f_px * dot(target_enu, u) / z;
        Some((x, y))
    }

    /// Vertical FOV implied by `hfov_deg` and the image aspect ratio.
    pub fn vfov_deg(&self) -> f64 {
        let f_px = self.focal_px();
        2.0 * (self.height as f64 / 2.0 / f_px).atan().to_degrees()
    }
}

/// A rectangle in pixel space, used for label-overlap testing.
#[derive(Debug, Clone, Copy)]
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
        };
        // East-of-north and slightly north: right of center, screen x > 500.
        let (x, _) = cam.project([50.0, 100.0, 0.0]).unwrap();
        assert!(x > 500.0, "expected right-of-center, got x={x}");
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
