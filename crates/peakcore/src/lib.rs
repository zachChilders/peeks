//! Transport-free core shared by the desktop (`peaklab`) and mobile (`src-tauri`) apps:
//! WGS84 geodesy, camera projection/label layout, Overpass query/parse, DEM tile
//! indexing/decoding, and terrain-occlusion raycasting.
//!
//! No network/filesystem I/O here on purpose — `peaklab` uses blocking `reqwest` 0.12
//! and a disk tile cache, `src-tauri` uses async `reqwest` 0.13 and a Tauri-managed
//! cache dir, and neither should have to pull in the other's HTTP/fs stack (or force a
//! version unification) just to share this math. [`dem::decode_tile`] decodes tile bytes
//! already in memory, which is pure computation, not I/O.

pub mod dem;
pub mod geo;
pub mod overpass;
pub mod peakfile;
pub mod projection;
pub mod skyline;
pub mod visibility;
