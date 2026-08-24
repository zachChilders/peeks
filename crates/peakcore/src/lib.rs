//! Transport-free core shared by the desktop (`peaklab`) and mobile (`src-tauri`) apps:
//! WGS84 geodesy, camera projection/label layout, and Overpass query/parse.
//!
//! No I/O here on purpose — `peaklab` uses blocking `reqwest` 0.12, `src-tauri` uses
//! async `reqwest` 0.13, and neither should have to pull in the other's HTTP stack (or
//! force a version unification) just to share this math.

pub mod geo;
pub mod overpass;
pub mod projection;
