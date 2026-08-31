//! Detecting the sky/terrain boundary in a camera frame, and fitting the DEM horizon to
//! it to recover camera pose error.
//!
//! The AR overlay is otherwise open-loop: [`crate::visibility::horizon_at_azimuth`]
//! computes where terrain *should* be from position and orientation alone, and any error
//! in that orientation lands straight on screen with nothing to correct it. Compass
//! heading is the weak link — a few degrees of magnetic interference slides the whole
//! overlay sideways and nothing notices. This module gives the image a vote.
//!
//! Everything here works on a plain grayscale slice rather than an image type: peakcore
//! compiles into the iOS static lib and stays free of the `image` crate.
//!
//! Both halves are deliberately dumb-but-robust — a step detector plus a 1-D dynamic
//! program, and a grid search rather than gradient descent. A wrong correction is worse
//! than no correction, so the confidence gates in [`fit`] matter more than the fit itself.

use crate::geo;
use crate::projection::{self, CameraPose};

/// Nominal range for turning a horizon look-angle back into a direction vector. Scale is
/// irrelevant to a pinhole projection; only the direction matters. Matches the constant
/// `Scene::project` uses for the same purpose.
const HORIZON_RANGE_M: f64 = 50_000.0;

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct DetectConfig {
    /// Height of the brightness windows sampled above and below a candidate row.
    pub window_px: usize,
    /// Cost per pixel of vertical movement between adjacent columns. Higher values
    /// prefer a flatter skyline; too high and it cannot follow a real ridge.
    pub continuity_penalty: f64,
    /// Largest vertical step considered between adjacent columns.
    pub max_jump_px: usize,
    /// Minimum above-minus-below contrast, in grey levels, for a column to be trusted.
    /// Columns below this still take part in the path (continuity needs them) but do not
    /// vote in the fit.
    pub min_contrast: f64,
}

impl Default for DetectConfig {
    fn default() -> Self {
        Self {
            window_px: 6,
            continuity_penalty: 1.5,
            max_jump_px: 16,
            min_contrast: 8.0,
        }
    }
}

/// Box-average a grayscale image down to `dst_w` x `dst_h`.
///
/// Averaging rather than nearest-neighbour matters here: the detector keys on the
/// brightness step across the skyline, and point-sampling a high-resolution frame aliases
/// thin bright features (a snow patch, a lit cloud edge) into that step. The device side
/// must reduce frames the same way for a desktop-tuned config to transfer.
pub fn downsample_gray(src: &[u8], src_w: usize, src_h: usize, dst_w: usize, dst_h: usize) -> Vec<u8> {
    if dst_w == 0 || dst_h == 0 || src_w == 0 || src_h == 0 || src.len() < src_w * src_h {
        return vec![0; dst_w * dst_h];
    }
    let mut out = vec![0u8; dst_w * dst_h];
    for dy in 0..dst_h {
        let y0 = dy * src_h / dst_h;
        let y1 = (((dy + 1) * src_h).div_ceil(dst_h)).min(src_h).max(y0 + 1);
        for dx in 0..dst_w {
            let x0 = dx * src_w / dst_w;
            let x1 = (((dx + 1) * src_w).div_ceil(dst_w)).min(src_w).max(x0 + 1);
            let mut sum = 0u32;
            let mut n = 0u32;
            for y in y0..y1 {
                for x in x0..x1 {
                    sum += u32::from(src[y * src_w + x]);
                    n += 1;
                }
            }
            out[dy * dst_w + dx] = (sum / n.max(1)) as u8;
        }
    }
    out
}

/// A detected skyline: the boundary row for each column, or `None` where the column had
/// too little contrast to trust.
#[derive(Debug, Clone)]
pub struct Skyline {
    pub rows: Vec<Option<u32>>,
    pub width: usize,
    pub height: usize,
}

impl Skyline {
    /// Fraction of columns with a trusted boundary.
    pub fn coverage(&self) -> f64 {
        if self.rows.is_empty() {
            return 0.0;
        }
        self.rows.iter().filter(|r| r.is_some()).count() as f64 / self.rows.len() as f64
    }
}

/// Find the sky/terrain boundary in a row-major grayscale image.
///
/// Scores each candidate row by how much brighter the band above it is than the band
/// below — which states "sky over terrain" directly, rather than keying on an edge that
/// terrain-internal detail produces just as strongly. A dynamic program then picks the
/// path across columns maximising total contrast minus a penalty on vertical movement;
/// that continuity constraint is what survives a bright cloud or a dark rock face in any
/// individual column.
pub fn detect(gray: &[u8], width: usize, height: usize, cfg: &DetectConfig) -> Skyline {
    let w = cfg.window_px;
    // Need a full window above and below, plus at least two candidate rows to choose from.
    if width == 0 || height < 2 * w + 2 || gray.len() < width * height {
        return Skyline {
            rows: vec![None; width],
            width,
            height,
        };
    }

    // Column-wise prefix sums, so a window mean is two lookups instead of `w` adds.
    // `prefix[(y + 1) * width + x]` is the sum of column `x` over rows `0..=y`.
    let mut prefix = vec![0u32; (height + 1) * width];
    for y in 0..height {
        for x in 0..width {
            prefix[(y + 1) * width + x] = prefix[y * width + x] + u32::from(gray[y * width + x]);
        }
    }
    let band = |x: usize, top: usize, bottom: usize| -> f64 {
        let sum = prefix[bottom * width + x] - prefix[top * width + x];
        f64::from(sum) / (bottom - top) as f64
    };

    let (y_lo, y_hi) = (w, height - w); // candidate rows are y_lo..y_hi
    let rows_considered = y_hi - y_lo;

    let score = |x: usize, y: usize| -> f64 { band(x, y - w, y) - band(x, y, y + w) };

    // Dynamic program left to right. `best[i]` is the score of the best path ending at
    // candidate row `y_lo + i` in the current column; `back` records where it came from.
    let mut best: Vec<f64> = (0..rows_considered).map(|i| score(0, y_lo + i)).collect();
    let mut prev: Vec<f64> = vec![0.0; rows_considered];
    let mut back = vec![0u32; width * rows_considered];

    for x in 1..width {
        std::mem::swap(&mut best, &mut prev);
        for i in 0..rows_considered {
            let lo = i.saturating_sub(cfg.max_jump_px);
            let hi = (i + cfg.max_jump_px + 1).min(rows_considered);
            let mut best_from = lo;
            let mut best_val = f64::NEG_INFINITY;
            for (offset, &from) in prev[lo..hi].iter().enumerate() {
                let j = lo + offset;
                let v = from - cfg.continuity_penalty * (i as f64 - j as f64).abs();
                if v > best_val {
                    best_val = v;
                    best_from = j;
                }
            }
            best[i] = best_val + score(x, y_lo + i);
            back[x * rows_considered + i] = best_from as u32;
        }
    }

    // Backtrack from the best endpoint.
    let mut i = best
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).expect("scores are finite"))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let mut path = vec![0usize; width];
    for x in (0..width).rev() {
        path[x] = y_lo + i;
        i = back[x * rows_considered + i] as usize;
    }

    // Drop columns whose own contrast is too weak to vote, even though the path ran
    // through them.
    let rows = path
        .iter()
        .enumerate()
        .map(|(x, &y)| (score(x, y) >= cfg.min_contrast).then_some(y as u32))
        .collect();

    Skyline {
        rows,
        width,
        height,
    }
}

// ---------------------------------------------------------------------------
// Fitting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct FitConfig {
    pub yaw_range_deg: f64,
    pub pitch_range_deg: f64,
    pub coarse_step_deg: f64,
    pub fine_step_deg: f64,
    /// Minimum fraction of columns that must have both a detection and a prediction.
    pub min_coverage: f64,
    /// Largest acceptable trimmed RMS residual, in frame pixels.
    pub max_rms_px: f64,
    /// How much worse the best *distant* alignment must be than the chosen one. Guards
    /// against repetitive ridgelines that match equally well at several yaw offsets.
    pub min_uniqueness: f64,
    /// Yaw separation, in degrees, beyond which an alternative counts as "distant" for
    /// the uniqueness test.
    pub uniqueness_separation_deg: f64,
}

impl Default for FitConfig {
    fn default() -> Self {
        Self {
            yaw_range_deg: 20.0,
            pitch_range_deg: 10.0,
            coarse_step_deg: 0.5,
            fine_step_deg: 0.05,
            min_coverage: 0.4,
            max_rms_px: 8.0,
            min_uniqueness: 1.25,
            uniqueness_separation_deg: 3.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fit {
    pub d_yaw_deg: f64,
    pub d_pitch_deg: f64,
    pub rms_px: f64,
    pub coverage: f64,
    pub uniqueness: f64,
}

/// Why a fit was not applied. Worth distinguishing so the debug HUD can say which gate
/// rejected a frame rather than just going quiet.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Reject {
    /// Not enough detected columns overlapped the projected horizon.
    Coverage { got: f64, needed: f64 },
    /// Best alignment still did not match well.
    Residual { got: f64, needed: f64 },
    /// A distant yaw offset matched nearly as well — repetitive terrain.
    Ambiguous { got: f64, needed: f64 },
    /// Nothing to fit: empty horizon or no detected columns.
    NoData,
}

/// Project the horizon through `pose` (with the given offsets applied) and return, for
/// each detected column, the predicted row.
///
/// `focal_px` is the *frame*-space focal length. Callers must not pass the screen-space
/// one from [`crate::projection::CameraIntrinsics::focal_px`]: that folds in the
/// `resizeAspectFill` crop used for display, which the raw capture frame has not had
/// applied. See [`crate::projection::CameraIntrinsics::frame_focal_px`].
fn predict(
    horizon: &[(f64, f64)],
    pose: &CameraPose,
    focal_px: f64,
    d_yaw_deg: f64,
    d_pitch_deg: f64,
    scratch: &mut Vec<(f64, f64)>,
) {
    let corrected = CameraPose {
        yaw_deg: pose.yaw_deg + d_yaw_deg,
        pitch_deg: pose.pitch_deg + d_pitch_deg,
        ..*pose
    };
    let basis = corrected.basis();

    scratch.clear();
    for &(az, el) in horizon {
        let v = geo::enu_from_look_angles(az, el, HORIZON_RANGE_M);
        if let Some(xy) =
            projection::project_with_basis(v, basis, focal_px, pose.width, pose.height)
        {
            scratch.push(xy);
        }
    }
    scratch.sort_by(|a, b| a.0.partial_cmp(&b.0).expect("projected coords are finite"));
}

/// Trimmed RMS residual between the detected skyline and a predicted one, plus the number
/// of columns that contributed. `None` when nothing overlapped.
fn residual(skyline: &Skyline, predicted: &[(f64, f64)], buf: &mut Vec<f64>) -> Option<(f64, usize)> {
    if predicted.len() < 2 {
        return None;
    }
    buf.clear();

    for (x, row) in skyline.rows.iter().enumerate() {
        let Some(y) = row else { continue };
        let xf = x as f64;
        // Outside the projected horizon's span there is nothing to compare against.
        if xf < predicted[0].0 || xf > predicted[predicted.len() - 1].0 {
            continue;
        }
        let i = predicted.partition_point(|p| p.0 < xf).max(1);
        let (x0, y0) = predicted[i - 1];
        let (x1, y1) = predicted[i];
        let t = if (x1 - x0).abs() < 1e-9 {
            0.0
        } else {
            (xf - x0) / (x1 - x0)
        };
        buf.push((y0 + t * (y1 - y0) - f64::from(*y)).abs());
    }

    if buf.is_empty() {
        return None;
    }
    let contributing = buf.len();

    // Trim the worst fifth before scoring. Clouds, a foreground tree, and the frame edge
    // all produce a handful of large residuals that would otherwise dominate the sum and
    // drag the whole fit toward them.
    buf.sort_by(|a, b| a.partial_cmp(b).expect("residuals are finite"));
    let keep = (buf.len() * 4 / 5).max(1);
    let sum_sq: f64 = buf[..keep].iter().map(|r| r * r).sum();
    Some(((sum_sq / keep as f64).sqrt(), contributing))
}

/// Solve for the yaw and pitch offsets that best align `horizon` to `skyline`.
///
/// Only yaw and pitch: focal length now comes from real device intrinsics and roll from
/// gravity, and letting a fit absorb error into either would hide a bug rather than
/// correct one.
///
/// A grid search rather than gradient descent, because a skyline can genuinely match at
/// several yaw offsets when the terrain repeats. A local method would happily converge on
/// one and report success; the grid sees them all, which is what makes the uniqueness gate
/// possible.
///
/// Accuracy is floored by the frame's angular resolution, not by `fine_step_deg`: the
/// detected skyline is quantised to whole rows, so a downsampled frame buys precision
/// with pixels. At 160 px across a 60 deg field one pixel is ~0.4 deg, and a systematic
/// half-pixel bias would be ~0.2 deg of pitch. Averaging over many columns recovers a good
/// deal of that, but downsampling harder directly costs angular accuracy.
pub fn fit(
    skyline: &Skyline,
    horizon: &[(f64, f64)],
    pose: &CameraPose,
    focal_px: f64,
    cfg: &FitConfig,
) -> Result<Fit, Reject> {
    let detected = skyline.rows.iter().filter(|r| r.is_some()).count();
    if horizon.is_empty() || detected == 0 {
        return Err(Reject::NoData);
    }

    let mut scratch = Vec::with_capacity(horizon.len());
    let mut buf = Vec::with_capacity(skyline.width);

    // Coarse pass. Every evaluation is kept so the uniqueness test can look at the whole
    // surface rather than just the winner's neighbourhood.
    let mut evaluated: Vec<(f64, f64, f64)> = Vec::new(); // (d_yaw, d_pitch, rms)
    let mut best: Option<(f64, f64, f64, usize)> = None; // + contributing columns

    let steps = |range: f64, step: f64| -> i32 { (range / step).round() as i32 };
    let (ny, np) = (
        steps(cfg.yaw_range_deg, cfg.coarse_step_deg),
        steps(cfg.pitch_range_deg, cfg.coarse_step_deg),
    );
    for iy in -ny..=ny {
        let dy = f64::from(iy) * cfg.coarse_step_deg;
        for ip in -np..=np {
            let dp = f64::from(ip) * cfg.coarse_step_deg;
            predict(horizon, pose, focal_px, dy, dp, &mut scratch);
            if let Some((rms, n)) = residual(skyline, &scratch, &mut buf) {
                evaluated.push((dy, dp, rms));
                if best.is_none_or(|(_, _, b, _)| rms < b) {
                    best = Some((dy, dp, rms, n));
                }
            }
        }
    }

    let Some((cy, cp, _, _)) = best else {
        return Err(Reject::NoData);
    };

    // Uniqueness, measured on the coarse surface: how much worse is the best alignment
    // that is *not* a small perturbation of the chosen one?
    let best_rms = evaluated
        .iter()
        .filter(|(dy, dp, _)| (*dy - cy).abs() < 1e-9 && (*dp - cp).abs() < 1e-9)
        .map(|(_, _, r)| *r)
        .next()
        .expect("the winner was evaluated");
    let rival = evaluated
        .iter()
        .filter(|(dy, _, _)| (dy - cy).abs() >= cfg.uniqueness_separation_deg)
        .map(|(_, _, r)| *r)
        .fold(f64::INFINITY, f64::min);
    // With no distant rival at all the alignment is trivially unique.
    let uniqueness = if rival.is_finite() && best_rms > 1e-9 {
        rival / best_rms
    } else {
        f64::INFINITY
    };

    // Fine pass around the coarse winner.
    let span = cfg.coarse_step_deg;
    let fine = steps(span, cfg.fine_step_deg);
    let mut refined: Option<(f64, f64, f64, usize)> = None;
    for iy in -fine..=fine {
        let dy = cy + f64::from(iy) * cfg.fine_step_deg;
        for ip in -fine..=fine {
            let dp = cp + f64::from(ip) * cfg.fine_step_deg;
            predict(horizon, pose, focal_px, dy, dp, &mut scratch);
            if let Some((rms, n)) = residual(skyline, &scratch, &mut buf) {
                if refined.is_none_or(|(_, _, b, _)| rms < b) {
                    refined = Some((dy, dp, rms, n));
                }
            }
        }
    }
    let (d_yaw_deg, d_pitch_deg, rms_px, contributing) = refined.expect("coarse winner re-evaluates");

    let coverage = contributing as f64 / skyline.width as f64;
    if coverage < cfg.min_coverage {
        return Err(Reject::Coverage {
            got: coverage,
            needed: cfg.min_coverage,
        });
    }
    if rms_px > cfg.max_rms_px {
        return Err(Reject::Residual {
            got: rms_px,
            needed: cfg.max_rms_px,
        });
    }
    if uniqueness < cfg.min_uniqueness {
        return Err(Reject::Ambiguous {
            got: uniqueness,
            needed: cfg.min_uniqueness,
        });
    }

    Ok(Fit {
        d_yaw_deg,
        d_pitch_deg,
        rms_px,
        coverage,
        uniqueness,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 160;
    const H: usize = 284;

    /// A frame with a known skyline: bright above, dark below, plus a little texture so
    /// the detector isn't handed a noiseless step.
    fn synthetic_frame(boundary: impl Fn(usize) -> usize) -> (Vec<u8>, Vec<usize>) {
        let mut img = vec![0u8; W * H];
        let truth: Vec<usize> = (0..W).map(&boundary).collect();
        for x in 0..W {
            let b = truth[x];
            for y in 0..H {
                // Deterministic pseudo-texture, so the test stays reproducible.
                let noise = ((x * 7 + y * 13) % 11) as i32 - 5;
                let base = if y < b { 200 } else { 60 };
                img[y * W + x] = (base + noise).clamp(0, 255) as u8;
            }
        }
        (img, truth)
    }

    fn frame_pose(yaw: f64, pitch: f64) -> CameraPose {
        CameraPose {
            yaw_deg: yaw,
            pitch_deg: pitch,
            roll_deg: 0.0,
            hfov_deg: 60.0,
            width: W as u32,
            height: H as u32,
            intrinsics: None,
        }
    }

    #[test]
    fn detects_a_flat_horizon() {
        let (img, truth) = synthetic_frame(|_| 120);
        let sky = detect(&img, W, H, &DetectConfig::default());
        assert!(sky.coverage() > 0.95, "coverage {}", sky.coverage());
        for (x, row) in sky.rows.iter().enumerate() {
            let y = row.expect("every column has strong contrast") as usize;
            assert!(
                y.abs_diff(truth[x]) <= 2,
                "column {x}: got {y}, want {}",
                truth[x]
            );
        }
    }

    #[test]
    fn follows_a_sloping_ridge() {
        let (img, truth) = synthetic_frame(|x| 90 + x / 3);
        let sky = detect(&img, W, H, &DetectConfig::default());
        assert!(sky.coverage() > 0.9, "coverage {}", sky.coverage());
        let worst = sky
            .rows
            .iter()
            .enumerate()
            .filter_map(|(x, r)| r.map(|y| (y as usize).abs_diff(truth[x])))
            .max()
            .unwrap();
        assert!(worst <= 3, "worst column error {worst}px");
    }

    #[test]
    fn featureless_frame_yields_no_confident_columns() {
        // Uniform grey: nothing to find, and the detector must say so rather than
        // inventing a boundary. This is the indoors-pointing-at-a-wall case.
        let img = vec![128u8; W * H];
        let sky = detect(&img, W, H, &DetectConfig::default());
        assert_eq!(sky.coverage(), 0.0, "expected no confident columns");
    }

    #[test]
    fn tiny_images_do_not_panic() {
        for (w, h) in [(0, 0), (1, 1), (160, 4), (4, 160)] {
            let img = vec![128u8; w * h];
            let sky = detect(&img, w, h, &DetectConfig::default());
            assert_eq!(sky.rows.len(), w);
        }
    }

    /// Build a skyline by projecting a horizon through a known pose — the inverse of what
    /// `fit` does, so a round trip should recover exactly the offset we introduced.
    fn skyline_from(horizon: &[(f64, f64)], pose: &CameraPose, focal: f64) -> Skyline {
        let mut scratch = Vec::new();
        predict(horizon, pose, focal, 0.0, 0.0, &mut scratch);
        let mut rows = vec![None; W];
        for (x, row) in rows.iter_mut().enumerate() {
            let xf = x as f64;
            if scratch.len() < 2 || xf < scratch[0].0 || xf > scratch[scratch.len() - 1].0 {
                continue;
            }
            let i = scratch.partition_point(|p| p.0 < xf).max(1);
            let (x0, y0) = scratch[i - 1];
            let (x1, y1) = scratch[i];
            let t = (xf - x0) / (x1 - x0);
            let y = y0 + t * (y1 - y0);
            if (0.0..H as f64).contains(&y) {
                // Round, don't truncate: a half-pixel bias here is a ~0.2 deg pitch
                // error at this frame's focal length, which would swamp the tolerance.
                *row = Some(y.round() as u32);
            }
        }
        Skyline {
            rows,
            width: W,
            height: H,
        }
    }

    /// A ridgeline with enough shape to pin down yaw unambiguously.
    fn varied_horizon() -> Vec<(f64, f64)> {
        (0..180)
            .map(|i| {
                let az = f64::from(i) * 2.0;
                let el = 3.0 * (az.to_radians() * 3.0).sin() + 1.5 * (az.to_radians() * 7.0).cos();
                (az, el)
            })
            .collect()
    }

    #[test]
    fn recovers_a_known_yaw_and_pitch_offset() {
        let horizon = varied_horizon();
        let truth = frame_pose(90.0, 0.0);
        let focal = truth.focal_px();
        let observed = skyline_from(&horizon, &truth, focal);

        // The pose the device *thinks* it has, off by a compass error and a pitch bias.
        let believed = frame_pose(90.0 - 4.0, 0.0 - 1.5);
        let fit = fit(&observed, &horizon, &believed, focal, &FitConfig::default())
            .expect("should fit a clean synthetic skyline");

        assert!(
            (fit.d_yaw_deg - 4.0).abs() < 0.15,
            "yaw: got {}, want 4.0",
            fit.d_yaw_deg
        );
        assert!(
            (fit.d_pitch_deg - 1.5).abs() < 0.15,
            "pitch: got {}, want 1.5",
            fit.d_pitch_deg
        );
        assert!(fit.rms_px < 2.0, "rms {}", fit.rms_px);
    }

    #[test]
    fn rejects_when_nothing_was_detected() {
        let horizon = varied_horizon();
        let empty = Skyline {
            rows: vec![None; W],
            width: W,
            height: H,
        };
        assert_eq!(
            fit(&empty, &horizon, &frame_pose(90.0, 0.0), 200.0, &FitConfig::default()),
            Err(Reject::NoData)
        );
    }

    #[test]
    fn rejects_a_skyline_that_does_not_match() {
        // A detected boundary that is flat where the horizon is not: no offset aligns it.
        let horizon = varied_horizon();
        let pose = frame_pose(90.0, 0.0);
        let flat = Skyline {
            rows: (0..W).map(|_| Some(20u32)).collect(),
            width: W,
            height: H,
        };
        let cfg = FitConfig {
            max_rms_px: 4.0,
            ..Default::default()
        };
        match fit(&flat, &horizon, &pose, pose.focal_px(), &cfg) {
            Err(Reject::Residual { .. }) | Err(Reject::Ambiguous { .. }) => {}
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn rejects_a_repetitive_ridgeline_as_ambiguous() {
        // Perfectly periodic in azimuth, so several yaw offsets align equally well. The
        // fit will find *an* answer; the uniqueness gate is what must refuse it.
        //
        // The period has to be shorter than the search range or there is no alias to be
        // confused by: at 30 cycles per revolution it repeats every 12 deg, so offsets of
        // +/-12 deg alias inside the +/-20 deg search.
        let horizon: Vec<(f64, f64)> = (0..180)
            .map(|i| {
                let az = f64::from(i) * 2.0;
                (az, 4.0 * (az.to_radians() * 30.0).sin())
            })
            .collect();
        let truth = frame_pose(90.0, 0.0);
        let focal = truth.focal_px();
        let observed = skyline_from(&horizon, &truth, focal);
        let believed = frame_pose(88.0, 0.0);

        match fit(&observed, &horizon, &believed, focal, &FitConfig::default()) {
            Err(Reject::Ambiguous { .. }) => {}
            other => panic!("expected Ambiguous for a periodic ridge, got {other:?}"),
        }
    }
}
