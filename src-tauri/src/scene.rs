//! Precomputed AR scene state, backing the `set_scene` / `project_labels` command pair.
//!
//! `set_scene` runs once per observer position: it resolves every peak to an ENU vector
//! and a great-circle distance, and sorts nearest-first. `project_labels` then runs on
//! a ~100ms tick and does only a [`CameraPose::basis`] plus a handful of dot products per
//! peak — the expensive ECEF round trip that the old TypeScript redid on every tick for
//! every peak now happens exactly once per observer move.

use peakcore::geo::{self, Geodetic};
use peakcore::projection::{self, layout_labels, CameraPose, Rect};
use serde::{Deserialize, Serialize};
use specta::Type;
use specta_typescript::Number;
use std::collections::HashMap;
use std::sync::Mutex;

/// A peak plus the browser-measured pixel size of its label text. Canvas text
/// measurement is a browser API with no Rust equivalent, so metrics are measured
/// client-side once (when the peak list loads) and shipped in here rather than
/// re-measured every tick.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PeakWithMetrics {
    // OSM node IDs are ordinary i64s well under 2^53, but specta forbids exporting
    // i64 as a bare TS `number` by default (precision loss for values that don't fit);
    // this opts back in rather than exporting a lossless-but-awkward `bigint`.
    #[specta(type = Number)]
    pub osm_id: i64,
    pub name: String,
    pub geo: Geodetic,
    pub text_w: f64,
    pub text_h: f64,
}

/// One placed (or unplaced) label. Keyed by `osm_id`, not `name` — duplicate summit
/// names are common in OSM ("Bald Mountain", "Black Butte"), and a name-keyed React
/// list would collide.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PlacedLabel {
    #[specta(type = Number)]
    pub osm_id: i64,
    pub name: String,
    pub anchor: (f64, f64),
    pub rect: Option<Rect>,
}

struct Entry {
    osm_id: i64,
    name: String,
    enu: [f64; 3],
    text_w: f64,
    text_h: f64,
}

/// Screen-space margin, in pixels: lets a label attach to a peak whose anchor dot sits
/// just off-frame. Matches peaklab's `render` subcommand.
const MARGIN: f64 = 60.0;
const MAX_STACK: usize = 6;
const LABEL_LINE_GAP: f64 = 4.0;

/// Tauri-managed state: the current observer's peaks, precomputed and sorted
/// nearest-first. Empty until the first `set_scene` call.
#[derive(Default)]
pub struct Scene(Mutex<Vec<Entry>>);

impl Scene {
    fn set(&self, observer: Geodetic, peaks: Vec<PeakWithMetrics>) {
        let mut entries: Vec<(f64, Entry)> = peaks
            .into_iter()
            .map(|p| {
                let enu = geo::enu(observer, p.geo);
                let dist = geo::great_circle_distance(observer, p.geo);
                (
                    dist,
                    Entry {
                        osm_id: p.osm_id,
                        name: p.name,
                        enu,
                        text_w: p.text_w,
                        text_h: p.text_h,
                    },
                )
            })
            .collect();
        entries.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

        *self.0.lock().unwrap() = entries.into_iter().map(|(_, e)| e).collect();
    }

    fn project(&self, pose: &CameraPose) -> Vec<PlacedLabel> {
        let entries = self.0.lock().unwrap();
        let basis = pose.basis();
        let focal_px = pose.focal_px();

        // `entries` is already sorted nearest-first from `set`; filtering to the
        // on-screen subset preserves that order without re-sorting.
        let onscreen: Vec<(&Entry, (f64, f64))> = entries
            .iter()
            .filter_map(|e| {
                let (x, y) = projection::project_with_basis(
                    e.enu,
                    basis,
                    focal_px,
                    pose.width,
                    pose.height,
                )?;
                let visible = x >= -MARGIN
                    && x <= pose.width as f64 + MARGIN
                    && y >= -MARGIN
                    && y <= pose.height as f64 + MARGIN;
                visible.then_some((e, (x, y)))
            })
            .collect();

        let candidates: Vec<(String, (f64, f64))> =
            onscreen.iter().map(|(e, xy)| (e.name.clone(), *xy)).collect();

        // Duplicate names always measure identically (same string, same font), so a
        // name-keyed lookup is safe here even though it can't disambiguate which peak a
        // name belongs to — only the returned PlacedLabel needs that, via osm_id below.
        let widths: HashMap<&str, (f64, f64)> = onscreen
            .iter()
            .map(|(e, _)| (e.name.as_str(), (e.text_w, e.text_h)))
            .collect();

        let placed = layout_labels(&candidates, |name| widths[name], MAX_STACK, LABEL_LINE_GAP);

        onscreen
            .iter()
            .zip(placed.iter())
            .map(|((e, _), p)| PlacedLabel {
                osm_id: e.osm_id,
                name: e.name.clone(),
                anchor: p.anchor,
                rect: p.text_rect,
            })
            .collect()
    }
}

#[tauri::command]
#[specta::specta]
pub fn set_scene(observer: Geodetic, peaks: Vec<PeakWithMetrics>, scene: tauri::State<Scene>) {
    scene.set(observer, peaks);
}

#[tauri::command]
#[specta::specta]
pub fn project_labels(pose: CameraPose, scene: tauri::State<Scene>) -> Vec<PlacedLabel> {
    scene.project(&pose)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_scene(n: usize) -> Scene {
        let observer = Geodetic::new(46.7858, -121.7353, 1_647.0);
        let peaks: Vec<PeakWithMetrics> = (0..n)
            .map(|i| {
                // Spread peaks around a full circle at varying distance/elevation so the
                // benchmark exercises both the on-screen and culled-off-screen paths,
                // like a real scene does.
                let bearing = (i as f64) * (360.0 / n as f64);
                let dist_deg = 0.05 + (i % 50) as f64 * 0.01;
                PeakWithMetrics {
                    osm_id: i as i64,
                    name: format!("Peak {i}"),
                    geo: Geodetic::new(
                        observer.lat + dist_deg * bearing.to_radians().cos(),
                        observer.lon + dist_deg * bearing.to_radians().sin(),
                        1_500.0 + (i % 30) as f64 * 100.0,
                    ),
                    text_w: 60.0 + (i % 10) as f64 * 4.0,
                    text_h: 18.0,
                }
            })
            .collect();

        let scene = Scene::default();
        scene.set(observer, peaks);
        scene
    }

    #[test]
    fn project_runs_and_returns_placements() {
        let scene = synthetic_scene(300);
        let pose = CameraPose {
            yaw_deg: 190.0,
            pitch_deg: 5.0,
            roll_deg: 0.0,
            hfov_deg: 66.0,
            width: 1200,
            height: 900,
        };
        assert!(!scene.project(&pose).is_empty());
    }

    /// Compute-only timing for `Scene::project` (basis + N dot products + cull +
    /// layout), excluding IPC/JSON serialization — this sandbox has no display server
    /// to run the real WebView and measure the full `invoke()` round trip, so this is
    /// the closest available number. 300 peaks approximates a dense mountain range at
    /// the app's 100km radius.
    ///
    /// Measured here (this environment, 1000 calls, 300 peaks): ~162us/call debug,
    /// ~24us/call release. Both are well under the plan's ~5ms fallback threshold even
    /// before accounting for IPC/JSON serialization overhead — but this is compute cost
    /// only, not a measurement of the real invoke() round trip, which this sandbox has
    /// no display server to run. Confirm with `performance.now()` around
    /// `commands.projectLabels()` on a device before relying on this number (see the
    /// matching note in CameraView.tsx).
    #[test]
    fn project_compute_cost() {
        let scene = synthetic_scene(300);
        let pose = CameraPose {
            yaw_deg: 190.0,
            pitch_deg: 5.0,
            roll_deg: 0.0,
            hfov_deg: 66.0,
            width: 1200,
            height: 900,
        };

        // Warm up (allocator, branch predictor) before timing.
        for _ in 0..20 {
            scene.project(&pose);
        }

        let iterations = 1000;
        let start = std::time::Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(scene.project(&pose));
        }
        let elapsed = start.elapsed();
        let per_call_us = elapsed.as_micros() as f64 / iterations as f64;
        eprintln!(
            "Scene::project: {per_call_us:.2}us/call over {iterations} calls (300 peaks, {})",
            if cfg!(debug_assertions) { "debug" } else { "release" }
        );

        // Generous regression guard, not a tight bound: catches an accidental O(n^2) or
        // a lock held across the whole computation, not meant to fail on ordinary noise.
        assert!(
            per_call_us < 5_000.0,
            "Scene::project got much slower than expected: {per_call_us:.2}us/call"
        );
    }
}
