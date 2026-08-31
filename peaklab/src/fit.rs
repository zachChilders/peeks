//! Desktop tuning loop for skyline detection and horizon fitting.
//!
//! The algorithm lives in [`peakcore::skyline`]; this wires it to a real photograph so it
//! can be developed and tuned without a device in the loop. The app's capture button
//! already writes overlay-composited photos to the camera roll, so a real frame from a
//! known position is a few seconds of work to obtain.
//!
//! Fitting works on the *uncropped* frame, matching what the device will feed it — see
//! [`peakcore::projection::CameraIntrinsics::frame_focal_px`] for why that differs from
//! the focal length the on-screen overlay uses.

use anyhow::{Context, Result};
use image::{Rgba, RgbaImage};
use imageproc::drawing::draw_filled_circle_mut;
use std::path::Path;

use peakcore::geo::Geodetic;
use peakcore::projection::{CameraIntrinsics, CameraPose};
use peakcore::skyline::{self, DetectConfig, FitConfig};
use peakcore::visibility;

use crate::dem::Dem;

/// Matches the app's own sweep (`src-tauri/src/peaks.rs`), so a desktop fit sees the same
/// horizon the device would.
const HORIZON_AZIMUTH_STEP_DEG: f64 = 2.0;
const HORIZON_RAY_STEP_M: f64 = 60.0;

const DETECTED_COLOR: Rgba<u8> = Rgba([80, 220, 255, 255]);
const PREDICTED_COLOR: Rgba<u8> = Rgba([255, 90, 90, 255]);

/// Everything the fit produced, for reporting.
pub struct Report {
    pub frame_w: usize,
    pub frame_h: usize,
    pub coverage: f64,
    pub horizon_points: usize,
    pub outcome: Result<skyline::Fit, skyline::Reject>,
}

/// Luma of an RGBA image, using Rec. 601 weights.
fn to_gray(img: &RgbaImage) -> Vec<u8> {
    img.pixels()
        .map(|p| {
            let [r, g, b, _] = p.0;
            ((299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b)) / 1000) as u8
        })
        .collect()
}

/// Detect the skyline in `photo` and solve for the pose offsets that align the DEM
/// horizon to it.
///
/// `hfov_deg` is the *frame's* horizontal field of view. On device this comes from
/// `AVCaptureDevice.activeFormat.videoFieldOfView`; here it has to be supplied, because a
/// photo carries no reliable record of the lens that took it.
#[allow(clippy::too_many_arguments)]
pub fn run(
    dem: &mut Dem,
    photo: &Path,
    observer: Geodetic,
    yaw_deg: f64,
    pitch_deg: f64,
    roll_deg: f64,
    hfov_deg: f64,
    range_m: f64,
    work_width: usize,
    detect_cfg: &DetectConfig,
    fit_cfg: &FitConfig,
    overlay_out: Option<&Path>,
) -> Result<Report> {
    let source = image::open(photo)
        .with_context(|| format!("opening {}", photo.display()))?
        .to_rgba8();
    let (sw, sh) = source.dimensions();

    // Reduce to a working size. Smaller is faster but directly costs angular accuracy,
    // since the detected boundary is quantised to whole rows.
    let work_w = work_width.min(sw as usize);
    let work_h = ((work_w as f64) * f64::from(sh) / f64::from(sw)).round() as usize;
    let gray = skyline::downsample_gray(&to_gray(&source), sw as usize, sh as usize, work_w, work_h);

    let detected = skyline::detect(&gray, work_w, work_h, detect_cfg);

    dem.load_region(observer.lat, observer.lon, range_m)
        .context("loading DEM for the horizon sweep")?;
    let horizon = visibility::sweep_horizon(
        dem.core(),
        observer,
        range_m,
        HORIZON_AZIMUTH_STEP_DEG,
        HORIZON_RAY_STEP_M,
    );

    // The photo is the whole sensor frame, so its long axis carries the native FOV. Build
    // intrinsics describing that frame at full resolution, then ask for the focal length
    // at the working size.
    let (long_px, short_px) = if sw >= sh {
        (f64::from(sw), f64::from(sh))
    } else {
        (f64::from(sh), f64::from(sw))
    };
    let intrinsics = CameraIntrinsics {
        fov_deg: hfov_deg,
        zoom_factor: 1.0,
        buffer_long_px: long_px,
        buffer_short_px: short_px,
    };
    let work_long = work_w.max(work_h) as f64;
    let focal_px = intrinsics.frame_focal_px(work_long);

    let pose = CameraPose {
        yaw_deg,
        pitch_deg,
        roll_deg,
        hfov_deg,
        width: work_w as u32,
        height: work_h as u32,
        intrinsics: None,
    };

    let outcome = skyline::fit(&detected, &horizon, &pose, focal_px, fit_cfg);

    if let Some(out) = overlay_out {
        let applied = outcome.as_ref().ok().copied();
        draw_overlay(&source, &detected, &horizon, &pose, focal_px, applied, out)?;
    }

    Ok(Report {
        frame_w: work_w,
        frame_h: work_h,
        coverage: detected.coverage(),
        horizon_points: horizon.len(),
        outcome,
    })
}

/// Draw the detected skyline and the projected horizon over the photo, so a bad fit is
/// diagnosable by eye rather than by residual alone.
fn draw_overlay(
    source: &RgbaImage,
    detected: &skyline::Skyline,
    horizon: &[(f64, f64)],
    pose: &CameraPose,
    focal_px: f64,
    applied: Option<skyline::Fit>,
    out: &Path,
) -> Result<()> {
    let mut canvas = source.clone();
    let (sw, sh) = canvas.dimensions();
    let sx = f64::from(sw) / detected.width as f64;
    let sy = f64::from(sh) / detected.height as f64;
    let radius = ((sw / 400).max(2)) as i32;

    for (x, row) in detected.rows.iter().enumerate() {
        let Some(y) = row else { continue };
        let px = ((x as f64 + 0.5) * sx) as i32;
        let py = ((f64::from(*y) + 0.5) * sy) as i32;
        draw_filled_circle_mut(&mut canvas, (px, py), radius, DETECTED_COLOR);
    }

    // The horizon as the fit would place it: with corrections when one was accepted, raw
    // otherwise, so a rejected frame still shows how far off it was.
    let (dy, dp) = applied.map_or((0.0, 0.0), |f| (f.d_yaw_deg, f.d_pitch_deg));
    let corrected = CameraPose {
        yaw_deg: pose.yaw_deg + dy,
        pitch_deg: pose.pitch_deg + dp,
        ..*pose
    };
    let basis = corrected.basis();
    for &(az, el) in horizon {
        let v = peakcore::geo::enu_from_look_angles(az, el, 50_000.0);
        if let Some((x, y)) =
            peakcore::projection::project_with_basis(v, basis, focal_px, pose.width, pose.height)
        {
            let px = (x * sx) as i32;
            let py = (y * sy) as i32;
            if px >= 0 && py >= 0 && px < sw as i32 && py < sh as i32 {
                draw_filled_circle_mut(&mut canvas, (px, py), radius, PREDICTED_COLOR);
            }
        }
    }

    canvas
        .save(out)
        .with_context(|| format!("saving {}", out.display()))?;
    Ok(())
}
