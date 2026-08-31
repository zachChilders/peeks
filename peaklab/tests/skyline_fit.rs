//! End-to-end skyline fitting against real terrain.
//!
//! `peakcore`'s own tests use analytic horizons; this one sweeps the actual Copernicus
//! DEM at Mammoth Lakes, renders that horizon into a synthetic frame, and checks the fit
//! recovers a deliberately introduced pose error through the real code path.
//!
//! Ignored by default: it needs the cached DEM tiles under `$PEAKLAB_DATA/dem`, which are
//! gitignored. Run with `cargo test -p peaklab -- --ignored`.

use peakcore::geo::Geodetic;
use peakcore::projection::{self, CameraIntrinsics, CameraPose};
use peakcore::skyline::{self, DetectConfig, FitConfig};
use peakcore::visibility;

use peaklab::dem::Dem;
use peaklab::{data_dir, EYE_HEIGHT_M};

/// The position the AR overlay was first debugged from, and the heading in that
/// screenshot.
const OBSERVER: (f64, f64) = (37.65214, -118.98018);
const YAW_DEG: f64 = 129.0;
const HFOV_DEG: f64 = 68.0;
const RANGE_M: f64 = 30_000.0;
const W: usize = 160;
const H: usize = 284;

/// Paint a frame from a horizon: bright above the boundary, dark below, with a little
/// deterministic texture so the detector isn't handed a noiseless step.
fn render_frame(horizon: &[(f64, f64)], pose: &CameraPose, focal_px: f64) -> Vec<u8> {
    let basis = pose.basis();
    let mut points: Vec<(f64, f64)> = horizon
        .iter()
        .filter_map(|&(az, el)| {
            let v = peakcore::geo::enu_from_look_angles(az, el, 50_000.0);
            projection::project_with_basis(v, basis, focal_px, pose.width, pose.height)
        })
        .collect();
    points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    assert!(points.len() >= 2, "horizon did not project into the frame");

    let mut img = vec![0u8; W * H];
    for x in 0..W {
        let xf = x as f64;
        let boundary = if xf < points[0].0 || xf > points[points.len() - 1].0 {
            H as f64 / 2.0
        } else {
            let i = points.partition_point(|p| p.0 < xf).max(1);
            let (x0, y0) = points[i - 1];
            let (x1, y1) = points[i];
            let t = if (x1 - x0).abs() < 1e-9 { 0.0 } else { (xf - x0) / (x1 - x0) };
            y0 + t * (y1 - y0)
        };
        for y in 0..H {
            let noise = ((x * 7 + y * 13) % 11) as i32 - 5;
            let base = if (y as f64) < boundary { 205 } else { 55 };
            img[y * W + x] = (base + noise).clamp(0, 255) as u8;
        }
    }
    img
}

fn setup() -> (Vec<(f64, f64)>, CameraPose, f64) {
    let mut dem = Dem::new(data_dir().join("dem"));
    dem.load_region(OBSERVER.0, OBSERVER.1, RANGE_M)
        .expect("DEM tiles for Mammoth must be cached; run a peaklab command there first");

    let ground = dem
        .elevation_at(OBSERVER.0, OBSERVER.1)
        .expect("no DEM coverage at the observer");
    let observer = Geodetic::new(OBSERVER.0, OBSERVER.1, ground + EYE_HEIGHT_M);

    let horizon = visibility::sweep_horizon(dem.core(), observer, RANGE_M, 2.0, 60.0);
    assert!(
        horizon.len() > 150,
        "expected a nearly complete sweep, got {} points",
        horizon.len()
    );

    let truth = CameraPose {
        yaw_deg: YAW_DEG,
        pitch_deg: 0.0,
        roll_deg: 0.0,
        hfov_deg: HFOV_DEG,
        width: W as u32,
        height: H as u32,
        intrinsics: None,
    };
    // The frame's long axis is its height, held portrait — see frame_focal_px.
    let focal_px = CameraIntrinsics {
        fov_deg: HFOV_DEG,
        zoom_factor: 1.0,
        buffer_long_px: H as f64,
        buffer_short_px: W as f64,
    }
    .frame_focal_px(H as f64);

    (horizon, truth, focal_px)
}

#[test]
#[ignore]
fn recovers_pose_error_against_real_terrain() {
    let (horizon, truth, focal_px) = setup();
    let frame = render_frame(&horizon, &truth, focal_px);

    let detected = skyline::detect(&frame, W, H, &DetectConfig::default());
    assert!(
        detected.coverage() > 0.8,
        "detector found only {:.0}% of columns on a synthetic frame",
        detected.coverage() * 100.0
    );

    // What a miscalibrated compass and a pitch bias would report.
    let (yaw_err, pitch_err) = (-3.5, 1.2);
    let believed = CameraPose {
        yaw_deg: truth.yaw_deg + yaw_err,
        pitch_deg: truth.pitch_deg + pitch_err,
        ..truth
    };

    let fit = skyline::fit(
        &detected,
        &horizon,
        &believed,
        focal_px,
        &FitConfig::default(),
    )
    .expect("real Sierra terrain should be unambiguous enough to fit");

    println!(
        "recovered yaw {:+.2} (want {:+.2}), pitch {:+.2} (want {:+.2}), rms {:.2}px, unique {:.2}x",
        fit.d_yaw_deg, -yaw_err, fit.d_pitch_deg, -pitch_err, fit.rms_px, fit.uniqueness
    );

    // One frame pixel is roughly 0.26 deg here, and the boundary is quantised to whole
    // rows, so sub-pixel agreement is the most that can be asked.
    assert!(
        (fit.d_yaw_deg - -yaw_err).abs() < 0.4,
        "yaw: got {:+.2}, want {:+.2}",
        fit.d_yaw_deg,
        -yaw_err
    );
    assert!(
        (fit.d_pitch_deg - -pitch_err).abs() < 0.4,
        "pitch: got {:+.2}, want {:+.2}",
        fit.d_pitch_deg,
        -pitch_err
    );
}

#[test]
#[ignore]
fn rejects_a_frame_with_no_skyline() {
    let (horizon, truth, focal_px) = setup();
    // Uniform grey: indoors, pointed at a wall.
    let blank = vec![128u8; W * H];
    let detected = skyline::detect(&blank, W, H, &DetectConfig::default());

    let outcome = skyline::fit(&detected, &horizon, &truth, focal_px, &FitConfig::default());
    assert!(
        outcome.is_err(),
        "a featureless frame must not produce a correction, got {outcome:?}"
    );
}
