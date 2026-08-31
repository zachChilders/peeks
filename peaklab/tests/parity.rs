//! Golden-file parity for the geo -> projection -> layout -> render pipeline.
//!
//! This exists to make the `peakcore` extraction provable: the same fixed scene must
//! produce byte-identical output before and after the move. It deliberately uses a
//! hardcoded peak table rather than the live pipeline — Overpass and the Copernicus DEM
//! are network dependencies, and a parity check that can't run offline isn't a check.
//!
//! The scene exercises every piece that moved: `geo::enu`, `geo::great_circle_distance`,
//! `CameraPose::project`, `layout_labels`, and `render::draw_labels`.

use peaklab::geo::{self, Geodetic};
use peaklab::projection::CameraPose;
use peaklab::render::{self, Candidate};
use sha2::{Digest, Sha256};

/// Fixed observer: Paradise, Mount Rainier NP, eye height above the surface.
const OBSERVER: (f64, f64, f64) = (46.7858, -121.7353, 1_647.0);

/// A fixed peak table standing in for a resolved Overpass + DEM fetch. Coordinates are
/// approximate — this is a fixture, not a data source. Names are deliberately varied in
/// length (label width drives the layout), and the Tatoosh cluster is tight enough to
/// force the stacking path.
const PEAKS: &[(&str, f64, f64, f64)] = &[
    ("Mount Rainier", 46.8523, -121.7603, 4392.0),
    ("Little Tahoma Peak", 46.8467, -121.7016, 3395.0),
    ("Pinnacle Peak", 46.7692, -121.7331, 1_910.0),
    ("Plummer Peak", 46.7660, -121.7404, 1_866.0),
    ("Unicorn Peak", 46.7708, -121.7017, 2_098.0),
    ("Mount Adams", 46.2024, -121.4909, 3_743.0),
    ("Mount St. Helens", 46.1912, -122.1944, 2_549.0),
    ("Eagle Peak", 46.7431, -121.7461, 1_853.0),
    ("Wahpenayo Peak", 46.7581, -121.8019, 1_926.0),
    ("Chutla Peak", 46.7458, -121.7789, 1_868.0),
    ("Denman Peak", 46.7690, -121.7455, 1_873.0),
    ("Lane Peak", 46.7620, -121.7419, 1_849.0),
];

const CAMERA: CameraPose = CameraPose {
    yaw_deg: 190.0,
    pitch_deg: 5.0,
    roll_deg: 0.0,
    hfov_deg: 66.0,
    width: 1200,
    height: 900,
    intrinsics: None,
};

/// Margin matching `peaklab render`: a label may attach to a dot just off-frame.
const MARGIN: f64 = 60.0;

/// Project the fixed scene, nearest-first, exactly as the `render` subcommand does
/// (minus the DEM-backed visibility filter, which has no offline input).
fn scene() -> Vec<Candidate> {
    let observer = Geodetic::new(OBSERVER.0, OBSERVER.1, OBSERVER.2);
    let mut onscreen: Vec<(f64, Candidate)> = Vec::new();

    for (name, lat, lon, elev) in PEAKS {
        let target = Geodetic::new(*lat, *lon, *elev);
        let v = geo::enu(observer, target);
        let Some((x, y)) = CAMERA.project(v) else {
            continue;
        };
        if x < -MARGIN
            || x > CAMERA.width as f64 + MARGIN
            || y < -MARGIN
            || y > CAMERA.height as f64 + MARGIN
        {
            continue;
        }
        onscreen.push((
            geo::great_circle_distance(observer, target),
            Candidate {
                label: (*name).to_string(),
                pixel: (x, y),
            },
        ));
    }

    onscreen.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    onscreen.into_iter().map(|(_, c)| c).collect()
}

/// Canonical text form of the layout, to 6 decimals — well below any plausible
/// refactor-induced drift, but tight enough to catch a changed formula.
fn placements_digest() -> String {
    let candidates = scene();
    let layout: Vec<(String, (f64, f64))> = candidates
        .iter()
        .map(|c| (c.label.clone(), c.pixel))
        .collect();

    // A synthetic metric, so the digest is font-independent and reproducible anywhere.
    let placed = peaklab::projection::layout_labels(
        &layout,
        |text| (text.chars().count() as f64 * 11.0, 22.0),
        6,
        4.0,
    );

    let mut canonical = String::new();
    for p in &placed {
        canonical.push_str(&format!("{}|{:.6}|{:.6}|", p.name, p.anchor.0, p.anchor.1));
        match p.text_rect {
            Some(r) => canonical.push_str(&format!(
                "{:.6},{:.6},{:.6},{:.6}\n",
                r.x, r.y, r.w, r.h
            )),
            None => canonical.push_str("none\n"),
        }
    }

    format!("{:x}", Sha256::digest(canonical.as_bytes()))
}

/// The whole geo/projection/layout pipeline, pinned. Captured on the pre-`peakcore`
/// tree; extracting the crate must not move a single digit.
#[test]
fn scene_placements_match_golden() {
    assert_eq!(
        placements_digest(),
        "7c6c5132575b6f8b695bd0a44efe16381af768a5c843a774a9b4b2943b827e89",
        "projection/layout output drifted from the golden scene"
    );
}

/// The rendered PNG, pinned. Requires a specific font to be byte-reproducible, so it
/// only runs where that font exists; `scene_placements_match_golden` is the portable
/// check and this one is the stronger, pixel-level confirmation on top of it.
const GOLDEN_FONT: &str = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf";

#[test]
fn rendered_png_matches_golden() {
    let Ok(font_bytes) = std::fs::read(GOLDEN_FONT) else {
        eprintln!("skipping: {GOLDEN_FONT} not present");
        return;
    };

    let mut canvas = render::blank_canvas(CAMERA.width, CAMERA.height);
    render::draw_labels(&mut canvas, &scene(), &font_bytes).unwrap();

    assert_eq!(
        format!("{:x}", Sha256::digest(canvas.as_raw())),
        "d6807690439148527f71eedb8d9d8bc3f369e44291f8c842de60253a995017c6",
        "rendered pixels drifted from the golden scene"
    );
}

/// Not an assertion — prints the scene so a failing golden test can be eyeballed.
/// Run with `cargo test -p peaklab --test parity -- --ignored --nocapture`.
#[test]
#[ignore = "diagnostic, not a check"]
fn dump_scene() {
    for c in scene() {
        eprintln!("{:<20} ({:9.3}, {:9.3})", c.label, c.pixel.0, c.pixel.1);
    }
}
