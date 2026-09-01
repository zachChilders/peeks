//! Continuous pose correction from the observed skyline.
//!
//! The AR overlay is otherwise open-loop — see [`peakcore::skyline`] for why that matters.
//! This module holds the feedback: camera frames arrive from the plugin, the detected
//! skyline is fitted against the DEM horizon, and the resulting yaw/pitch offsets are
//! applied to every subsequent projection.
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
/// Chosen to comfortably span the gap between a `record_pose` tick (~100ms) and the
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
    /// Offsets currently applied to the pose, in degrees.
    pub d_yaw_deg: f64,
    pub d_pitch_deg: f64,
    /// True once at least one fit has been accepted.
    pub locked: bool,
    /// Human-readable outcome of the most recent frame.
    pub detail: String,
    /// Frames fitted and frames accepted since the camera started.
    pub frames: u32,
    pub accepted: u32,
}

struct Inner {
    /// The most recent *uncorrected* pose from `project_labels`.
    ///
    /// Storing the raw pose is load-bearing: the fit solves for an offset relative to what
    /// the sensors reported, so feeding back an already-corrected pose would compound the
    /// correction on every frame and walk the overlay off the terrain.
    last_pose: Option<CameraPose>,
    last_intrinsics: Option<CameraIntrinsics>,
    /// `(recorded_at, yaw_deg, pitch_deg)` from recent `record_pose` calls, oldest first,
    /// trimmed to `POSE_HISTORY_MAX_AGE`. Used only to answer "has the phone been held
    /// steady lately" — see `is_stable`.
    pose_history: VecDeque<(Instant, f64, f64)>,
    d_yaw_deg: f64,
    d_pitch_deg: f64,
    locked: bool,
    detail: String,
    frames: u32,
    accepted: u32,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            last_pose: None,
            last_intrinsics: None,
            pose_history: VecDeque::new(),
            d_yaw_deg: 0.0,
            d_pitch_deg: 0.0,
            locked: false,
            detail: "waiting for frames".to_string(),
            frames: 0,
            accepted: 0,
        }
    }
}

#[derive(Default)]
pub struct Calibration(Mutex<Inner>);

impl Calibration {
    /// Record the raw pose the sensors reported, for the next frame to fit against.
    pub fn record_pose(&self, pose: &CameraPose) {
        let mut g = self.0.lock().unwrap();
        g.last_pose = Some(*pose);
        if pose.intrinsics.is_some() {
            g.last_intrinsics = pose.intrinsics;
        }

        let now = Instant::now();
        g.pose_history.push_back((now, pose.yaw_deg, pose.pitch_deg));
        while g
            .pose_history
            .front()
            .is_some_and(|&(at, _, _)| now.duration_since(at) > POSE_HISTORY_MAX_AGE)
        {
            g.pose_history.pop_front();
        }
    }

    /// Whether the phone has been roughly still for `STABILITY_WINDOW`, judged from
    /// `record_pose`'s own history rather than by correlating clocks with the frame's
    /// capture time — both ends of this comparison are Rust's own monotonic clock, so no
    /// cross-language timestamp reconciliation is needed.
    ///
    /// `false` when there isn't yet enough history to span the window: that's the
    /// conservative default, matching how the whole system already starts `locked: false`
    /// rather than optimistically trusting a single sample.
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

    /// Offsets to add to the sensor pose before projecting.
    pub fn offsets(&self) -> (f64, f64) {
        let g = self.0.lock().unwrap();
        (g.d_yaw_deg, g.d_pitch_deg)
    }

    pub fn status(&self) -> CalibrationStatus {
        let g = self.0.lock().unwrap();
        CalibrationStatus {
            d_yaw_deg: g.d_yaw_deg,
            d_pitch_deg: g.d_pitch_deg,
            locked: g.locked,
            detail: g.detail.clone(),
            frames: g.frames,
            accepted: g.accepted,
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

        let (pose, intrinsics, stable) = {
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
        let Some(pose) = pose else {
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
            ..pose
        };

        let detected = skyline::detect(&gray, width, height, &DetectConfig::default());
        let outcome = skyline::fit(
            &detected,
            horizon,
            &frame_pose,
            focal_px,
            &FitConfig::default(),
        );

        let mut g = self.0.lock().unwrap();
        g.frames += 1;
        match outcome {
            Ok(fit) => {
                // A fit far from the current estimate is more likely a misfit than a real
                // jump, so let it in slowly rather than not at all.
                let jump = (fit.d_yaw_deg - g.d_yaw_deg)
                    .abs()
                    .max((fit.d_pitch_deg - g.d_pitch_deg).abs());
                let alpha = if g.locked && jump > OUTLIER_DEG {
                    SMOOTHING / 4.0
                } else if g.locked {
                    SMOOTHING
                } else {
                    // Nothing to blend with on the first accepted fit.
                    1.0
                };
                g.d_yaw_deg += alpha * (fit.d_yaw_deg - g.d_yaw_deg);
                g.d_pitch_deg += alpha * (fit.d_pitch_deg - g.d_pitch_deg);
                g.locked = true;
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
                    Reject::Residual { got, .. } => format!("poor match ({got:.1}px)"),
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

    fn pose() -> CameraPose {
        CameraPose {
            yaw_deg: 90.0,
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

    #[test]
    fn starts_neutral_and_unlocked() {
        let c = Calibration::default();
        assert_eq!(c.offsets(), (0.0, 0.0));
        assert!(!c.status().locked);
    }

    /// Seeds enough backdated, near-identical pose history for `is_stable` to pass,
    /// without a real sleep — deterministic and fast, unlike waiting out
    /// `STABILITY_WINDOW` in real time.
    fn make_stable(c: &Calibration, pose: &CameraPose) {
        let mut g = c.0.lock().unwrap();
        g.last_pose = Some(*pose);
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
        assert_eq!(c.offsets(), (0.0, 0.0));
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
            g.last_pose = Some(p);
            g.last_intrinsics = p.intrinsics;
            let now = Instant::now();
            g.pose_history.push_back((now - STABILITY_WINDOW, 60.0, 0.0));
            g.pose_history.push_back((now, 100.0, 0.0));
        }
        let frame = base64::engine::general_purpose::STANDARD.encode([0u8; 4]);
        c.ingest_frame(&frame, 2, 2, &[(0.0, 0.0), (2.0, 0.0)]);
        assert_eq!(c.offsets(), (0.0, 0.0));
        assert!(
            c.status().detail.contains("steady"),
            "expected a motion rejection, got: {}",
            c.status().detail
        );
    }

    #[test]
    fn records_the_raw_pose_not_a_corrected_one() {
        // Guards the compounding bug: `record_pose` must be handed the sensor pose, so
        // the stored value has to come back unchanged regardless of applied offsets.
        let c = Calibration::default();
        c.record_pose(&pose());
        let stored = c.0.lock().unwrap().last_pose.unwrap();
        assert_eq!(stored.yaw_deg, 90.0);
        assert_eq!(stored.pitch_deg, 0.0);
    }

    #[test]
    fn malformed_frames_are_reported_not_panicked_on() {
        let c = Calibration::default();
        c.record_pose(&pose());
        c.ingest_frame("not base64!!", 4, 4, &[(0.0, 0.0)]);
        assert!(c.status().detail.contains("base64"));

        // Declared size larger than the payload.
        let tiny = base64::engine::general_purpose::STANDARD.encode([0u8; 4]);
        c.ingest_frame(&tiny, 100, 100, &[(0.0, 0.0)]);
        assert!(c.status().detail.contains("shorter"));
        assert_eq!(c.offsets(), (0.0, 0.0));
    }
}
