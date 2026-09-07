//! Where the camera is pointed: heading resolution, and pose correction from the observed
//! skyline.
//!
//! # Why the compass is not the heading
//!
//! Pitch and roll come from gravity, which is physically redundant — accelerometer and
//! gyro, fused and self-correcting — and measures within about a degree. Yaw had exactly
//! one source, the magnetometer, and nothing else in the phone knows where north is. A
//! screenshot from Long Valley measured the overlay at +7.4° of yaw error against 1.0° of
//! pitch error, which is the shape of the problem: "rotational error" is compass error.
//!
//! So the compass is not treated as the heading here. It is a *prior*: it seeds the
//! heading until the skyline fit produces an absolute answer, and after that it is out of
//! the loop entirely. Steady-state yaw is
//!
//! ```text
//! yaw(t) = relative_yaw(t) + north_offset
//! ```
//!
//! where `relative_yaw` is the gyro's integral about the local vertical (arbitrary origin,
//! smooth, no magnetometer — see `startMotionUpdates` in `CameraPlugin.swift`) and
//! `north_offset` is the datum solved by [`peakcore::skyline`] and then *held*.
//!
//! The important consequence is a change of failure mode, not of accuracy. Correcting a
//! compass reading requires a successful fit on every frame, and a fitter that stops
//! locking drops you straight back to the raw compass. Correcting a gyro datum requires
//! one successful fit ever; later fits only trim the slow drift, and a failed one costs
//! nothing because the previous answer still stands.
//!
//! Frames are consumed here in Rust rather than in the webview. The plugin's channels
//! support a Rust callback, so tens of kilobytes per frame never cross the IPC boundary.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use peakcore::geo;
use peakcore::projection::{CameraIntrinsics, CameraPose};
use peakcore::skyline::{self, DetectConfig, FitConfig, Reject};
use serde::Serialize;
use specta::Type;

/// Weight given to each newly accepted fit. Low enough that one unusual frame nudges
/// rather than yanks the overlay, high enough to converge in a few seconds at ~2 Hz.
const SMOOTHING: f64 = 0.35;

/// A single accepted fit further than this from the current estimate is treated as
/// suspect and applied at reduced weight — a real pose change moves the *sensors* too,
/// so a large jump in the residual offset usually means a misfit.
const OUTLIER_DEG: f64 = 5.0;

/// How far back `is_stable` looks to decide whether the phone has been held steady.
/// Chosen to comfortably span the gap between a `resolve_pose` tick (~100ms) and the
/// transport latency of a camera frame delivered from the native side (~500ms cadence
/// plus IPC), so that gap stops mattering: if the pose barely moved across this window,
/// it doesn't matter that we don't know its exact value at the frame's capture instant.
const STABILITY_WINDOW: Duration = Duration::from_millis(300);

/// Above this, `ingest_frame` skips the fit entirely rather than compute one. Chosen to
/// comfortably reject deliberate panning (tens of deg/sec) while passing ordinary hand
/// tremor while aiming (sub-1 deg/sec sustained) — averaging over `STABILITY_WINDOW` is
/// what keeps a momentary jitter spike from being misread as sustained motion.
const MAX_STABLE_ANGULAR_VELOCITY_DEG_PER_S: f64 = 5.0;

/// Trim `pose_history` beyond this so a long session doesn't grow it unbounded.
const POSE_HISTORY_MAX_AGE: Duration = Duration::from_millis(600);

/// What the fitter is currently doing, for the debug HUD. TestFlight builds have no
/// debugger, so "why is nothing being corrected" has to be answerable from a screenshot.
#[derive(Debug, Clone, Default, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CalibrationStatus {
    /// How far the resolved heading currently sits from what the compass is reporting, in
    /// degrees — i.e. how wrong the magnetometer is right now. Zero until the first fit is
    /// accepted, since the compass *is* the heading until then.
    pub d_yaw_deg: f64,
    /// Correction applied to the reported pitch, in degrees.
    pub d_pitch_deg: f64,
    /// True once at least one fit has been accepted and the heading is held on the gyro
    /// datum rather than read from the compass.
    pub locked: bool,
    /// Human-readable outcome of the most recent frame.
    pub detail: String,
    /// Frames fitted and frames accepted since the camera started.
    pub frames: u32,
    pub accepted: u32,
    /// Size of the last camera frame fitted, and the focal length used for it, in frame
    /// pixels. Reported because everything the fitter concludes rests on these three
    /// numbers being right, and nothing else on screen would reveal it if they were not:
    /// a capture buffer arriving in the sensor's native landscape rather than portrait,
    /// or intrinsics describing a different format than the one delivered, both show up
    /// only as a residual that never converges. Zero until a frame has been fitted.
    pub frame_w: u32,
    pub frame_h: u32,
    pub frame_focal_px: f64,
    /// Seconds since the last accepted fit, or `None` if there has not been one. This is
    /// how stale the held datum is, and therefore how much gyro drift has accumulated
    /// since anything last corrected it — the one cost of holding rather than re-reading.
    pub lock_age_s: Option<f64>,
}

/// One tick's resolved pose together with the sensor datums it was built from, so that an
/// accepted fit can be turned back into an absolute correction.
#[derive(Debug, Clone, Copy)]
struct PoseRecord {
    /// Exactly what was projected with, corrections included.
    pose: CameraPose,
    /// The gyro datum that `pose.yaw_deg` was derived from, when there was one.
    relative_yaw_deg: Option<f64>,
    /// Pitch as the sensors reported it, before `d_pitch_deg` was added.
    sensor_pitch_deg: f64,
}

struct Inner {
    /// The most recent pose from `project_labels`, *as projected*, plus the datums behind
    /// it.
    ///
    /// This is the corrected pose, not the raw sensor one, and the direction matters. A
    /// fit returns an offset relative to whatever pose it was handed, so fitting against
    /// the corrected pose is what makes the update `north_offset += d_yaw` — a correction
    /// to the held datum, converging geometrically as successive fits return smaller
    /// residual offsets. It does not compound, because the quantity being corrected is
    /// anchored to the gyro rather than re-derived from a compass reading that moves
    /// underneath it. That was the hazard the previous design had to store the *raw* pose
    /// to avoid, and inverting it is the point of resolving the heading here.
    last_pose: Option<PoseRecord>,
    last_intrinsics: Option<CameraIntrinsics>,
    /// `(recorded_at, yaw_deg, pitch_deg)` from recent `resolve_pose` calls, oldest first,
    /// trimmed to `POSE_HISTORY_MAX_AGE`. Used only to answer "has the phone been held
    /// steady lately" — see `is_stable`.
    pose_history: VecDeque<(Instant, f64, f64)>,
    /// Degrees to add to the gyro's `relative_yaw_deg` to get true-north heading. Seeded
    /// from the compass every tick until the first fit is accepted; held and refined by
    /// the fitter after that. `None` before any motion reading has arrived.
    north_offset_deg: Option<f64>,
    /// Yaw actually projected with, and the compass reading, at the last tick — kept only
    /// so the HUD can report how far apart they are.
    last_resolved_yaw_deg: f64,
    last_compass_yaw_deg: f64,
    d_pitch_deg: f64,
    locked: bool,
    last_accepted_at: Option<Instant>,
    detail: String,
    frames: u32,
    accepted: u32,
    frame_w: u32,
    frame_h: u32,
    frame_focal_px: f64,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            last_pose: None,
            last_intrinsics: None,
            pose_history: VecDeque::new(),
            north_offset_deg: None,
            last_resolved_yaw_deg: 0.0,
            last_compass_yaw_deg: 0.0,
            d_pitch_deg: 0.0,
            locked: false,
            last_accepted_at: None,
            detail: "waiting for frames".to_string(),
            frames: 0,
            accepted: 0,
            frame_w: 0,
            frame_h: 0,
            frame_focal_px: 0.0,
        }
    }
}

#[derive(Default)]
pub struct Calibration(Mutex<Inner>);

/// Wrap a heading into `[0, 360)`.
fn wrap360(deg: f64) -> f64 {
    deg.rem_euclid(360.0)
}

impl Calibration {
    /// Turn one tick's sensor readings into the pose to project with, and record it for
    /// the next frame to fit against.
    ///
    /// `sensor.yaw_deg` is the compass reading; `relative_yaw_deg` is the gyro's integral
    /// about the local vertical, or `None` on a device with no motion stream (in which
    /// case this degrades to the compass, exactly as before).
    ///
    /// Until a fit is accepted the returned yaw *is* the compass reading — the offset is
    /// re-seeded from it every tick, so nothing about the pre-lock overlay changes. After
    /// a fit is accepted the offset is held, and the compass stops contributing.
    pub fn resolve_pose(&self, sensor: &CameraPose, relative_yaw_deg: Option<f64>) -> CameraPose {
        let mut g = self.0.lock().unwrap();

        let yaw_deg = match (relative_yaw_deg, g.locked, g.north_offset_deg) {
            // Locked onto a fitted datum: the compass is not consulted at all.
            (Some(rel), true, Some(offset)) => wrap360(rel + offset),
            // Not locked yet (or no gyro to hold onto): follow the compass, and keep the
            // offset trailing it so the first fit starts from where the overlay already is.
            (Some(rel), _, _) => {
                g.north_offset_deg = Some(sensor.yaw_deg - rel);
                sensor.yaw_deg
            }
            (None, _, _) => sensor.yaw_deg,
        };

        let pose = CameraPose {
            yaw_deg,
            pitch_deg: sensor.pitch_deg + g.d_pitch_deg,
            ..*sensor
        };

        g.last_resolved_yaw_deg = yaw_deg;
        g.last_compass_yaw_deg = sensor.yaw_deg;
        g.last_pose = Some(PoseRecord {
            pose,
            relative_yaw_deg,
            sensor_pitch_deg: sensor.pitch_deg,
        });
        if sensor.intrinsics.is_some() {
            g.last_intrinsics = sensor.intrinsics;
        }

        let now = Instant::now();
        g.pose_history.push_back((now, yaw_deg, pose.pitch_deg));
        while g
            .pose_history
            .front()
            .is_some_and(|&(at, _, _)| now.duration_since(at) > POSE_HISTORY_MAX_AGE)
        {
            g.pose_history.pop_front();
        }

        pose
    }

    /// Whether the phone has been roughly still for `STABILITY_WINDOW`, judged from
    /// `resolve_pose`'s own history rather than by correlating clocks with the frame's
    /// capture time — both ends of this comparison are Rust's own monotonic clock, so no
    /// cross-language timestamp reconciliation is needed.
    ///
    /// `false` when there isn't yet enough history to span the window: that's the
    /// conservative default, matching how the whole system already starts `locked: false`
    /// rather than optimistically trusting a single sample.
    ///
    /// The history holds *resolved* yaw, which once locked is the gyro datum rather than
    /// the compass. That makes this measure real motion instead of magnetometer noise, so
    /// a jumpy compass can no longer spend the gate's budget on a phone that is holding
    /// still. Pre-lock it is the compass, and behaves as it always has.
    fn is_stable(history: &VecDeque<(Instant, f64, f64)>) -> bool {
        let (Some(&(oldest_at, oldest_yaw, oldest_pitch)), Some(&(newest_at, newest_yaw, newest_pitch))) =
            (history.front(), history.back())
        else {
            return false;
        };

        let dt = newest_at.duration_since(oldest_at).as_secs_f64();
        if dt < STABILITY_WINDOW.as_secs_f64() * 0.5 {
            return false;
        }

        let d_yaw = geo::angle_diff_deg(newest_yaw, oldest_yaw).abs();
        let d_pitch = (newest_pitch - oldest_pitch).abs();
        (d_yaw.max(d_pitch) / dt) <= MAX_STABLE_ANGULAR_VELOCITY_DEG_PER_S
    }

    pub fn status(&self) -> CalibrationStatus {
        let g = self.0.lock().unwrap();
        CalibrationStatus {
            d_yaw_deg: if g.locked {
                geo::angle_diff_deg(g.last_resolved_yaw_deg, g.last_compass_yaw_deg)
            } else {
                0.0
            },
            d_pitch_deg: g.d_pitch_deg,
            locked: g.locked,
            detail: g.detail.clone(),
            frames: g.frames,
            accepted: g.accepted,
            frame_w: g.frame_w,
            frame_h: g.frame_h,
            frame_focal_px: g.frame_focal_px,
            lock_age_s: g.last_accepted_at.map(|at| at.elapsed().as_secs_f64()),
        }
    }

    pub fn reset(&self) {
        *self.0.lock().unwrap() = Inner::default();
    }

    /// Detect the skyline in one frame and fold an accepted fit into the running estimate.
    ///
    /// `horizon` is the `(azimuth, elevation)` sweep held by the scene.
    pub fn ingest_frame(
        &self,
        gray_b64: &str,
        width: usize,
        height: usize,
        horizon: &[(f64, f64)],
    ) {
        let Ok(gray) = base64::engine::general_purpose::STANDARD.decode(gray_b64) else {
            self.note("frame was not valid base64");
            return;
        };
        if gray.len() < width * height {
            self.note("frame shorter than its declared size");
            return;
        }

        let (record, intrinsics, stable) = {
            let g = self.0.lock().unwrap();
            (g.last_pose, g.last_intrinsics, Self::is_stable(&g.pose_history))
        };
        // `last_pose` below is whatever the most recent ~100ms tick recorded, and this
        // frame's image may have been captured anywhere in the gap since (plus IPC
        // transport time). While the phone is still, that gap is a non-issue — the pose
        // barely changed either way. While panning, the gap has a consistent direction
        // for the whole gesture, so every frame during it feeds the EMA a similarly
        // biased "correction" that doesn't average out: this is the mechanism behind
        // reported drift that compounds instead of settling, worse at high zoom because
        // that's when panning to hunt for a peak happens most.
        if !stable {
            self.note("steady the phone to calibrate");
            return;
        }
        let Some(record) = record else {
            self.note("no pose yet");
            return;
        };
        let Some(intrinsics) = intrinsics else {
            // Without real intrinsics the focal length is a guess, and a fit against a
            // guessed focal length would quietly absorb that error into yaw and pitch.
            self.note("waiting for camera intrinsics");
            return;
        };
        if horizon.is_empty() {
            self.note("no horizon computed yet");
            return;
        }

        // Fit against the raw capture frame, which has not had the display's aspect-fill
        // crop applied — hence `frame_focal_px` rather than the screen-space `focal_px`.
        let frame_long = width.max(height) as f64;
        let focal_px = intrinsics.frame_focal_px(frame_long);
        let frame_pose = CameraPose {
            width: width as u32,
            height: height as u32,
            intrinsics: None,
            ..record.pose
        };

        let detected = skyline::detect(&gray, width, height, &DetectConfig::default());
        let outcome = skyline::fit(
            &detected,
            horizon,
            &frame_pose,
            focal_px,
            &FitConfig::default(),
        );

        self.record_outcome(record, width, height, focal_px, outcome);
    }

    /// Fold one frame's outcome into the running estimate.
    ///
    /// Split out from [`ingest_frame`](Self::ingest_frame) so the arithmetic that turns a
    /// fit into an absolute heading datum can be exercised directly, without synthesising
    /// a camera frame that detects and fits end to end.
    fn record_outcome(
        &self,
        record: PoseRecord,
        width: usize,
        height: usize,
        focal_px: f64,
        outcome: Result<skyline::Fit, Reject>,
    ) {
        let mut g = self.0.lock().unwrap();
        g.frames += 1;
        g.frame_w = width as u32;
        g.frame_h = height as u32;
        g.frame_focal_px = focal_px;
        match outcome {
            Ok(fit) => {
                // The fit's offsets are relative to the pose the frame was projected with,
                // so converting them into absolute datums means adding them back onto that
                // pose and subtracting the sensor readings behind it. Doing it from the
                // record rather than from the live offsets keeps the update exact even if
                // several ticks (and another accepted fit) have landed in between.
                let target_north = record
                    .relative_yaw_deg
                    .map(|rel| record.pose.yaw_deg + fit.d_yaw_deg - rel);
                let target_pitch =
                    record.pose.pitch_deg + fit.d_pitch_deg - record.sensor_pitch_deg;

                // A fit far from the current estimate is more likely a misfit than a real
                // jump, so let it in slowly rather than not at all.
                let yaw_jump = match (target_north, g.north_offset_deg) {
                    (Some(t), Some(cur)) => geo::angle_diff_deg(t, cur).abs(),
                    _ => 0.0,
                };
                let jump = yaw_jump.max((target_pitch - g.d_pitch_deg).abs());
                let alpha = if g.locked && jump > OUTLIER_DEG {
                    SMOOTHING / 4.0
                } else if g.locked {
                    SMOOTHING
                } else {
                    // Nothing to blend with on the first accepted fit.
                    1.0
                };

                if let (Some(target), Some(cur)) = (target_north, g.north_offset_deg) {
                    // Blended the long way round otherwise: the offset is an angle, and
                    // 359° and 1° are two degrees apart, not 358.
                    g.north_offset_deg = Some(cur + alpha * geo::angle_diff_deg(target, cur));
                } else {
                    g.north_offset_deg = target_north.or(g.north_offset_deg);
                }
                g.d_pitch_deg += alpha * (target_pitch - g.d_pitch_deg);

                // Only a fit that produced a heading datum counts as a lock. Without a
                // gyro reading there is nothing to hold the correction onto, so the
                // compass has to stay in the loop.
                if record.relative_yaw_deg.is_some() {
                    g.locked = true;
                }
                g.last_accepted_at = Some(Instant::now());
                g.accepted += 1;
                g.detail = format!(
                    "fit {:+.1}/{:+.1}° rms {:.1}px cover {:.0}%",
                    fit.d_yaw_deg,
                    fit.d_pitch_deg,
                    fit.rms_px,
                    fit.coverage * 100.0
                );
            }
            Err(reject) => {
                g.detail = match reject {
                    Reject::Coverage { got, .. } => {
                        format!("no skyline ({:.0}% of columns)", got * 100.0)
                    }
                    Reject::Residual {
                        got,
                        needed,
                        d_yaw_deg,
                        d_pitch_deg,
                    } => format!(
                        "poor match {got:.1}px (>{needed:.0}) at {d_yaw_deg:+.1}/{d_pitch_deg:+.1}°"
                    ),
                    Reject::Ambiguous { got, .. } => format!("ambiguous ridge ({got:.1}x)"),
                    Reject::NoData => "nothing to fit".to_string(),
                };
            }
        }
    }

    fn note(&self, detail: &str) {
        self.0.lock().unwrap().detail = detail.to_string();
    }
}

/// Begin consuming camera frames and correcting the pose. Call after the camera is
/// running; the capture device has to exist before frames can be delivered.
///
/// The callback runs on the plugin's channel thread and does the whole detect-and-fit
/// there, so no frame data reaches the webview.
#[tauri::command]
#[specta::specta]
pub fn start_calibration(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    use tauri_plugin_camera::{CameraExt, FrameEvent};

    app.state::<Calibration>().reset();

    let handle = app.clone();
    app.camera()
        .start_frame_updates(move |event| match event {
            FrameEvent::Reading(frame) => {
                let horizon = crate::scene::horizon_snapshot(&handle.state::<crate::scene::Scene>());
                handle.state::<Calibration>().ingest_frame(
                    &frame.gray,
                    frame.width,
                    frame.height,
                    &horizon,
                );
            }
            FrameEvent::Error(err) => {
                handle.state::<Calibration>().note(&format!("frame error: {err}"));
            }
        })
        .map(|_id| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn stop_calibration(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    use tauri_plugin_camera::CameraExt;

    app.state::<Calibration>().reset();
    app.camera().stop_frame_updates().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compass reading for a tick. Yaw is what CoreLocation reported, not what the
    /// overlay ends up pointed by.
    fn sensor_pose(compass_yaw: f64) -> CameraPose {
        CameraPose {
            yaw_deg: compass_yaw,
            pitch_deg: 0.0,
            roll_deg: 0.0,
            hfov_deg: 60.0,
            width: 390,
            height: 844,
            intrinsics: Some(CameraIntrinsics {
                fov_deg: 68.0,
                zoom_factor: 1.0,
                buffer_long_px: 1920.0,
                buffer_short_px: 1080.0,
            }),
        }
    }

    fn pose() -> CameraPose {
        sensor_pose(90.0)
    }

    /// A fit the gates would have accepted, offering `d_yaw`/`d_pitch` corrections.
    fn accepted(d_yaw: f64, d_pitch: f64) -> skyline::Fit {
        skyline::Fit {
            d_yaw_deg: d_yaw,
            d_pitch_deg: d_pitch,
            rms_px: 1.0,
            coverage: 0.9,
            uniqueness: 3.0,
        }
    }

    /// Drive one tick and then one accepted fit against it, as the real loop does.
    fn tick_and_fit(c: &Calibration, compass_yaw: f64, rel_yaw: Option<f64>, fit: skyline::Fit) {
        let resolved = c.resolve_pose(&sensor_pose(compass_yaw), rel_yaw);
        let record = c.0.lock().unwrap().last_pose.expect("just recorded");
        assert_eq!(record.pose.yaw_deg, resolved.yaw_deg);
        c.record_outcome(record, 160, 284, 106.6, Ok(fit));
    }

    #[test]
    fn starts_neutral_and_unlocked() {
        let c = Calibration::default();
        assert!(!c.status().locked);
        assert_eq!(c.status().d_yaw_deg, 0.0);
        assert_eq!(c.status().lock_age_s, None);
    }

    #[test]
    fn before_any_fit_the_overlay_follows_the_compass_exactly() {
        // The gyro datum is being tracked, but nothing has told us where north is yet, so
        // there is nothing better than the compass to point at. This is the pre-lock
        // behaviour the app has always had, and it must not change.
        let c = Calibration::default();
        for (compass, rel) in [(90.0, 0.0), (95.0, 4.0), (88.0, -3.0)] {
            let resolved = c.resolve_pose(&sensor_pose(compass), Some(rel));
            assert_eq!(resolved.yaw_deg, compass);
        }
        assert!(!c.status().locked);
    }

    #[test]
    fn a_locked_heading_ignores_the_compass_and_follows_the_gyro() {
        // The whole point of the change: after one fit, yaw is `relative + offset`, and a
        // compass that wanders — or is simply 7 degrees wrong, as measured in the field —
        // no longer moves the overlay.
        let c = Calibration::default();
        // Tick at compass 200, gyro datum 0, and a fit that says true north is 207.4.
        tick_and_fit(&c, 200.0, Some(0.0), accepted(7.4, 0.0));
        assert!(c.status().locked);

        // Compass now swings 15 degrees with the phone completely still. Nothing moves.
        let held = c.resolve_pose(&sensor_pose(215.0), Some(0.0));
        assert!(
            (held.yaw_deg - 207.4).abs() < 1e-9,
            "compass leaked into a locked heading: {}",
            held.yaw_deg
        );

        // Pan 30 degrees right. The overlay follows the gyro, one for one.
        let panned = c.resolve_pose(&sensor_pose(180.0), Some(30.0));
        assert!(
            (panned.yaw_deg - 237.4).abs() < 1e-9,
            "expected the pan to carry the datum, got {}",
            panned.yaw_deg
        );
    }

    #[test]
    fn repeated_fits_converge_instead_of_compounding() {
        // The regression guard for the failure this design replaces. Each fit is computed
        // against the pose actually projected with, so once the datum is right the fitter
        // reports no further correction and the heading stops moving. A loop that
        // compounded would walk off instead.
        const TRUE_NORTH_OFFSET: f64 = 187.4;
        let c = Calibration::default();

        // The phone pans slowly throughout. The compass tracks the pan but reads a
        // constant 7.4 degrees low, which is the field-measured error.
        let mut rel = 0.0;
        for _ in 0..12 {
            let compass = 180.0 + rel;
            let resolved = c.resolve_pose(&sensor_pose(compass), Some(rel));
            // What a fit against this frame would find: the gap between where we are
            // pointed and the truth.
            let d_yaw = geo::angle_diff_deg(rel + TRUE_NORTH_OFFSET, resolved.yaw_deg);
            let record = c.0.lock().unwrap().last_pose.expect("just recorded");
            c.record_outcome(record, 160, 284, 106.6, Ok(accepted(d_yaw, 0.0)));
            rel += 0.5;
        }

        let resolved = c.resolve_pose(&sensor_pose(180.0 + rel), Some(rel));
        assert!(
            geo::angle_diff_deg(resolved.yaw_deg, rel + TRUE_NORTH_OFFSET).abs() < 0.01,
            "expected convergence on the true heading, got {} want {}",
            resolved.yaw_deg,
            rel + TRUE_NORTH_OFFSET
        );
        // And the correction the HUD reports is the compass error, not zero.
        assert!((c.status().d_yaw_deg - 7.4).abs() < 0.01, "{}", c.status().d_yaw_deg);
    }

    #[test]
    fn the_offset_blends_the_short_way_around_north() {
        // A datum near 360 corrected towards 2 must move two degrees forward, not 358
        // back. Plain arithmetic on the offsets gets this wrong; angle_diff_deg does not.
        let c = Calibration::default();
        tick_and_fit(&c, 359.0, Some(0.0), accepted(0.0, 0.0));
        assert!(c.status().locked);
        // Now a fit that wants +3 degrees, crossing the wrap.
        tick_and_fit(&c, 359.0, Some(0.0), accepted(3.0, 0.0));

        let resolved = c.resolve_pose(&sensor_pose(359.0), Some(0.0));
        let expected = 359.0 + SMOOTHING * 3.0;
        assert!(
            geo::angle_diff_deg(resolved.yaw_deg, expected).abs() < 1e-9,
            "got {} want {expected}",
            resolved.yaw_deg
        );
        assert!(
            (0.0..360.0).contains(&resolved.yaw_deg),
            "heading left the [0,360) range: {}",
            resolved.yaw_deg
        );
    }

    #[test]
    fn without_a_gyro_reading_it_falls_back_to_the_compass_and_never_locks() {
        // A device with no motion stream has nothing to hold a datum onto, so the compass
        // must stay in the loop rather than the overlay freezing on a stale heading.
        let c = Calibration::default();
        tick_and_fit(&c, 200.0, None, accepted(7.4, 0.0));
        assert!(!c.status().locked, "locked without a heading datum to hold");
        assert_eq!(c.resolve_pose(&sensor_pose(215.0), None).yaw_deg, 215.0);
    }

    #[test]
    fn pitch_correction_is_absolute_not_cumulative() {
        // Pitch keeps its own offset against gravity, which has an absolute reference and
        // does not drift. Applying the same fit twice must not double the correction.
        let c = Calibration::default();
        tick_and_fit(&c, 90.0, Some(0.0), accepted(0.0, 2.0));
        assert!((c.status().d_pitch_deg - 2.0).abs() < 1e-9);

        // Second tick already carries the +2; a fit that now finds nothing left to correct
        // must leave it there.
        tick_and_fit(&c, 90.0, Some(0.0), accepted(0.0, 0.0));
        assert!(
            (c.status().d_pitch_deg - 2.0).abs() < 1e-9,
            "pitch offset drifted to {}",
            c.status().d_pitch_deg
        );
        assert!((c.resolve_pose(&sensor_pose(90.0), Some(0.0)).pitch_deg - 2.0).abs() < 1e-9);
    }

    #[test]
    fn lock_age_reports_how_stale_the_datum_is() {
        // Gyro drift is the cost of holding rather than re-reading, and it grows with time
        // since the last correction. Nothing else on screen would show it.
        let c = Calibration::default();
        assert_eq!(c.status().lock_age_s, None);
        tick_and_fit(&c, 90.0, Some(0.0), accepted(1.0, 0.0));
        assert!(c.status().lock_age_s.is_some_and(|s| s < 1.0));
    }

    /// Seeds enough backdated, near-identical pose history for `is_stable` to pass,
    /// without a real sleep — deterministic and fast, unlike waiting out
    /// `STABILITY_WINDOW` in real time.
    fn make_stable(c: &Calibration, pose: &CameraPose) {
        let mut g = c.0.lock().unwrap();
        g.last_pose = Some(PoseRecord {
            pose: *pose,
            relative_yaw_deg: Some(0.0),
            sensor_pitch_deg: pose.pitch_deg,
        });
        if pose.intrinsics.is_some() {
            g.last_intrinsics = pose.intrinsics;
        }
        let now = Instant::now();
        g.pose_history.push_back((now - STABILITY_WINDOW, pose.yaw_deg, pose.pitch_deg));
        g.pose_history.push_back((now, pose.yaw_deg, pose.pitch_deg));
    }

    #[test]
    fn refuses_to_fit_without_intrinsics() {
        // A guessed focal length would let the fit absorb focal error into yaw and pitch,
        // which is exactly the failure the intrinsics work removed.
        let c = Calibration::default();
        make_stable(
            &c,
            &CameraPose {
                intrinsics: None,
                ..pose()
            },
        );
        // A correctly-sized frame, so this reaches the intrinsics check rather than
        // stopping at the length validation.
        let frame = base64::engine::general_purpose::STANDARD.encode([0u8; 4]);
        c.ingest_frame(&frame, 2, 2, &[(0.0, 0.0), (2.0, 0.0)]);
        assert!(!c.status().locked);
        assert!(c.status().detail.contains("intrinsics"));
    }

    #[test]
    fn is_stable_false_with_no_or_insufficient_history() {
        assert!(!Calibration::is_stable(&VecDeque::new()));

        let mut history = VecDeque::new();
        history.push_back((Instant::now(), 90.0, 0.0));
        // A single sample spans zero time, well under half of STABILITY_WINDOW.
        assert!(!Calibration::is_stable(&history));
    }

    #[test]
    fn ingest_frame_proceeds_past_the_stability_gate_when_still() {
        // Stable history but no intrinsics: if this reaches the intrinsics rejection
        // rather than the motion one, the stability gate correctly let it through.
        let c = Calibration::default();
        make_stable(
            &c,
            &CameraPose {
                intrinsics: None,
                ..pose()
            },
        );
        let frame = base64::engine::general_purpose::STANDARD.encode([0u8; 4]);
        c.ingest_frame(&frame, 2, 2, &[(0.0, 0.0), (2.0, 0.0)]);
        assert!(
            c.status().detail.contains("intrinsics"),
            "expected to reach the intrinsics check, got: {}",
            c.status().detail
        );
    }

    #[test]
    fn ingest_frame_rejects_for_motion_even_with_everything_else_valid() {
        // Pose swinging 40 degrees across the stability window -- well over a deliberate
        // pan, let alone hand tremor -- with otherwise perfectly valid intrinsics and
        // horizon. The regression test for the reported compounding-drift bug: this must
        // never reach the fitter at all.
        let c = Calibration::default();
        {
            let mut g = c.0.lock().unwrap();
            let p = pose();
            g.last_pose = Some(PoseRecord {
                pose: p,
                relative_yaw_deg: Some(0.0),
                sensor_pitch_deg: p.pitch_deg,
            });
            g.last_intrinsics = p.intrinsics;
            let now = Instant::now();
            g.pose_history.push_back((now - STABILITY_WINDOW, 60.0, 0.0));
            g.pose_history.push_back((now, 100.0, 0.0));
        }
        let frame = base64::engine::general_purpose::STANDARD.encode([0u8; 4]);
        c.ingest_frame(&frame, 2, 2, &[(0.0, 0.0), (2.0, 0.0)]);
        assert!(!c.status().locked);
        assert!(
            c.status().detail.contains("steady"),
            "expected a motion rejection, got: {}",
            c.status().detail
        );
    }

    #[test]
    fn records_the_pose_it_projected_with_and_the_datums_behind_it() {
        // The fit runs against the pose the frame was drawn with, and converting its
        // offsets back into absolute datums needs the sensor readings that produced it.
        let c = Calibration::default();
        tick_and_fit(&c, 200.0, Some(0.0), accepted(7.4, 0.0));
        let resolved = c.resolve_pose(&sensor_pose(180.0), Some(12.0));

        let record = c.0.lock().unwrap().last_pose.expect("just recorded");
        assert_eq!(record.pose.yaw_deg, resolved.yaw_deg);
        assert_eq!(record.relative_yaw_deg, Some(12.0));
        assert_eq!(record.sensor_pitch_deg, 0.0);
    }

    #[test]
    fn malformed_frames_are_reported_not_panicked_on() {
        let c = Calibration::default();
        c.resolve_pose(&pose(), Some(0.0));
        c.ingest_frame("not base64!!", 4, 4, &[(0.0, 0.0)]);
        assert!(c.status().detail.contains("base64"));

        // Declared size larger than the payload.
        let tiny = base64::engine::general_purpose::STANDARD.encode([0u8; 4]);
        c.ingest_frame(&tiny, 100, 100, &[(0.0, 0.0)]);
        assert!(c.status().detail.contains("shorter"));
        assert!(!c.status().locked);
    }
}
