//! Drawing peak labels onto an image (the second half of M4 — [`crate::projection`]
//! computes where things go, this module puts pixels on a canvas).

use ab_glyph::{FontRef, PxScale};
use anyhow::{Context, Result};
use image::{Rgba, RgbaImage};
use imageproc::drawing::{draw_filled_circle_mut, draw_line_segment_mut, draw_text_mut, text_size};

use crate::projection::{layout_labels, PlacedLabel};

const LABEL_COLOR: Rgba<u8> = Rgba([255, 255, 40, 255]);
const DOT_COLOR: Rgba<u8> = Rgba([255, 60, 60, 255]);
const LEADER_COLOR: Rgba<u8> = Rgba([255, 255, 255, 180]);
const FONT_SIZE: f32 = 22.0;

/// Common macOS system font locations, tried in order. This tool only ever runs on the
/// author's desktop, so a hardcoded search list is fine — no need to bundle a font.
const FONT_CANDIDATES: &[&str] = &[
    "/System/Library/Fonts/Supplemental/Arial.ttf",
    "/System/Library/Fonts/Helvetica.ttc",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
];

pub fn load_font(explicit: Option<&std::path::Path>) -> Result<Vec<u8>> {
    if let Some(path) = explicit {
        return std::fs::read(path).with_context(|| format!("reading font {}", path.display()));
    }
    for candidate in FONT_CANDIDATES {
        if let Ok(bytes) = std::fs::read(candidate) {
            return Ok(bytes);
        }
    }
    anyhow::bail!(
        "no font found in {:?}; pass --font <path-to-ttf>",
        FONT_CANDIDATES
    )
}

/// A synthetic sky-gradient canvas, for testing the projection/layout math before a real
/// photo is in the loop.
pub fn blank_canvas(width: u32, height: u32) -> RgbaImage {
    RgbaImage::from_fn(width, height, |_, y| {
        let t = y as f32 / height as f32;
        let top = (110.0, 150.0, 200.0);
        let bottom = (200.0, 205.0, 200.0);
        Rgba([
            (top.0 + (bottom.0 - top.0) * t) as u8,
            (top.1 + (bottom.1 - top.1) * t) as u8,
            (top.2 + (bottom.2 - top.2) * t) as u8,
            255,
        ])
    })
}

/// One peak ready to be drawn: its display name and projected pixel position.
pub struct Candidate {
    pub label: String,
    pub pixel: (f64, f64),
}

/// Lay out and draw labels for every candidate onto `canvas`, nearest-first (callers
/// should pre-sort `candidates` by distance ascending so closer peaks win contested
/// screen space). Returns the placements actually used, for callers that want to report
/// how many labels got skipped for lack of room.
pub fn draw_labels(
    canvas: &mut RgbaImage,
    candidates: &[Candidate],
    font_bytes: &[u8],
) -> Result<Vec<PlacedLabel>> {
    let font = FontRef::try_from_slice(font_bytes).context("parsing font")?;
    let scale = PxScale::from(FONT_SIZE);

    let layout_input: Vec<(String, (f64, f64))> = candidates
        .iter()
        .map(|c| (c.label.clone(), c.pixel))
        .collect();

    let placed = layout_labels(
        &layout_input,
        |text| {
            let (w, h) = text_size(scale, &font, text);
            (w as f64, h as f64)
        },
        6,
        4.0,
    );

    for label in &placed {
        draw_filled_circle_mut(
            canvas,
            (label.anchor.0.round() as i32, label.anchor.1.round() as i32),
            4,
            DOT_COLOR,
        );

        let Some(rect) = label.text_rect else { continue };

        let text_bottom = rect.y + rect.h;
        let leader_start = (rect.x + rect.w / 2.0, text_bottom + 2.0);
        if (leader_start.1 - label.anchor.1).abs() > 6.0 {
            draw_line_segment_mut(
                canvas,
                (leader_start.0 as f32, leader_start.1 as f32),
                (label.anchor.0 as f32, label.anchor.1 as f32),
                LEADER_COLOR,
            );
        }

        draw_text_mut(
            canvas,
            LABEL_COLOR,
            rect.x.round() as i32,
            rect.y.round() as i32,
            scale,
            &font,
            &label.name,
        );
    }

    Ok(placed)
}
