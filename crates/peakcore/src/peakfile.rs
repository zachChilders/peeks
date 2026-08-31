//! The bundled named-peak dataset: binary format, writer, and reader.
//!
//! Replaces the runtime Overpass query. Overpass only ever contributed a name, an OSM
//! id, and a coordinate accurate enough to seed [`crate::dem::Dem::local_max`]'s ±30 m
//! snap search — every geometric value the app actually displays comes from the DEM. A
//! gazetteer that small ships as a file: all 690k named peaks on Earth fit in ~20 MB,
//! which removes a per-launch network dependency from an app used where there is no
//! signal.
//!
//! Records are bucketed into the same 1°×1° tiles as the DEM, so a lookup reuses
//! [`crate::dem::tiles_for_region`] rather than defining a second region enumerator, and
//! only the handful of tiles a query touches are decoded.
//!
//! Layout, little-endian throughout:
//!
//! ```text
//! header   magic "MVPK" | version: u32 | tile_count: u32
//! index    tile_count × { lat0: i16, lon0: i16, offset: u32, count: u32 }
//! records  per tile, contiguous: { osm_id: i64, lat: f32, lon: f32, name_len: u8, name }
//! ```
//!
//! The index is sorted by `(lat0, lon0)` so a tile is found by binary search. `offset` is
//! absolute from the start of the file.
//!
//! Coordinates are `f32`, which resolves to about a metre at these magnitudes — two
//! orders of magnitude tighter than the snap window that consumes them, and it halves the
//! per-record cost versus `f64`.

use crate::dem;
use crate::geo::{self, Geodetic};

const MAGIC: &[u8; 4] = b"MVPK";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 12;
const INDEX_ENTRY_LEN: usize = 12;
/// `name_len` is a single byte. The longest name in a 58,740-peak Alpine sample was 117
/// bytes, so this is headroom rather than a real constraint.
const MAX_NAME_LEN: usize = 255;

/// One named peak. `lat`/`lon` are `f64` for callers' convenience but round-trip through
/// `f32` on disk, so a parsed record sits within about a metre of what was written.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub osm_id: i64,
    pub lat: f64,
    pub lon: f64,
    pub name: String,
}

#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("not a peak file: bad magic")]
    BadMagic,
    #[error("unsupported peak file version {0} (expected {VERSION})")]
    Version(u32),
    #[error("peak file truncated at byte {0}")]
    Truncated(usize),
    #[error("peak file contains a non-UTF-8 name at byte {0}")]
    Utf8(usize),
}

/// The 1°×1° tile a coordinate belongs to, matching [`crate::dem::tiles_for_region`]'s
/// convention: latitude rows run north→south, so the tile with south edge `lat0` covers
/// `(lat0, lat0 + 1]`.
pub fn tile_of(lat: f64, lon: f64) -> (i32, i32) {
    ((lat.ceil() as i32) - 1, lon.floor() as i32)
}

/// Truncate to at most [`MAX_NAME_LEN`] bytes without splitting a UTF-8 character.
fn truncate_name(name: &str) -> &str {
    if name.len() <= MAX_NAME_LEN {
        return name;
    }
    let mut end = MAX_NAME_LEN;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// Serialize records into the bundled format, grouping them into 1°×1° tiles.
///
/// Input order is irrelevant — records are bucketed and the index is sorted here.
pub fn write(records: &[Record]) -> Vec<u8> {
    // Bucket first so each tile's records land contiguously, then sort the tile keys so
    // the reader can binary-search the index.
    let mut buckets: std::collections::HashMap<(i32, i32), Vec<&Record>> =
        std::collections::HashMap::new();
    for r in records {
        buckets.entry(tile_of(r.lat, r.lon)).or_default().push(r);
    }
    let mut keys: Vec<(i32, i32)> = buckets.keys().copied().collect();
    keys.sort_unstable();

    let index_len = keys.len() * INDEX_ENTRY_LEN;
    let mut header = Vec::with_capacity(HEADER_LEN + index_len);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&VERSION.to_le_bytes());
    header.extend_from_slice(&(keys.len() as u32).to_le_bytes());

    // Records are built alongside the index so each entry can record the absolute offset
    // its tile starts at, which is only known once the preceding tiles are laid out.
    let mut index = Vec::with_capacity(index_len);
    let mut body: Vec<u8> = Vec::new();
    for key in &keys {
        let tile = &buckets[key];
        let offset = HEADER_LEN + index_len + body.len();

        index.extend_from_slice(&(key.0 as i16).to_le_bytes());
        index.extend_from_slice(&(key.1 as i16).to_le_bytes());
        index.extend_from_slice(&(offset as u32).to_le_bytes());
        index.extend_from_slice(&(tile.len() as u32).to_le_bytes());

        for r in tile {
            let name = truncate_name(&r.name);
            body.extend_from_slice(&r.osm_id.to_le_bytes());
            body.extend_from_slice(&(r.lat as f32).to_le_bytes());
            body.extend_from_slice(&(r.lon as f32).to_le_bytes());
            body.push(name.len() as u8);
            body.extend_from_slice(name.as_bytes());
        }
    }

    let mut out = header;
    out.extend_from_slice(&index);
    out.extend_from_slice(&body);
    out
}

#[derive(Debug, Clone, Copy)]
struct TileEntry {
    lat0: i32,
    lon0: i32,
    offset: usize,
    count: usize,
}

/// A parsed peak file. Holds the raw bytes and decodes a tile's records only when a
/// query touches it — the whole point of the tile index, since a global file has ~690k
/// records and eagerly building that many `String`s would cost more than the file itself.
pub struct PeakFile {
    bytes: Vec<u8>,
    index: Vec<TileEntry>,
}

impl PeakFile {
    pub fn parse(bytes: Vec<u8>) -> Result<Self, FormatError> {
        if bytes.len() < HEADER_LEN {
            return Err(FormatError::Truncated(bytes.len()));
        }
        if &bytes[0..4] != MAGIC {
            return Err(FormatError::BadMagic);
        }
        let version = u32::from_le_bytes(bytes[4..8].try_into().expect("4 bytes"));
        if version != VERSION {
            return Err(FormatError::Version(version));
        }
        let tile_count = u32::from_le_bytes(bytes[8..12].try_into().expect("4 bytes")) as usize;

        let index_end = HEADER_LEN + tile_count * INDEX_ENTRY_LEN;
        if bytes.len() < index_end {
            return Err(FormatError::Truncated(bytes.len()));
        }

        let mut index = Vec::with_capacity(tile_count);
        for i in 0..tile_count {
            let at = HEADER_LEN + i * INDEX_ENTRY_LEN;
            index.push(TileEntry {
                lat0: i16::from_le_bytes(bytes[at..at + 2].try_into().expect("2 bytes")) as i32,
                lon0: i16::from_le_bytes(bytes[at + 2..at + 4].try_into().expect("2 bytes")) as i32,
                offset: u32::from_le_bytes(bytes[at + 4..at + 8].try_into().expect("4 bytes"))
                    as usize,
                count: u32::from_le_bytes(bytes[at + 8..at + 12].try_into().expect("4 bytes"))
                    as usize,
            });
        }

        Ok(Self { bytes, index })
    }

    /// Total records across every tile.
    pub fn len(&self) -> usize {
        self.index.iter().map(|t| t.count).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Decode one tile's records.
    fn tile_records(&self, entry: &TileEntry) -> Result<Vec<Record>, FormatError> {
        let mut out = Vec::with_capacity(entry.count);
        let mut at = entry.offset;
        for _ in 0..entry.count {
            if at + 17 > self.bytes.len() {
                return Err(FormatError::Truncated(at));
            }
            let osm_id = i64::from_le_bytes(self.bytes[at..at + 8].try_into().expect("8 bytes"));
            let lat = f32::from_le_bytes(self.bytes[at + 8..at + 12].try_into().expect("4 bytes"));
            let lon = f32::from_le_bytes(self.bytes[at + 12..at + 16].try_into().expect("4 bytes"));
            let name_len = self.bytes[at + 16] as usize;
            at += 17;

            if at + name_len > self.bytes.len() {
                return Err(FormatError::Truncated(at));
            }
            let name = std::str::from_utf8(&self.bytes[at..at + name_len])
                .map_err(|_| FormatError::Utf8(at))?
                .to_string();
            at += name_len;

            out.push(Record {
                osm_id,
                lat: lat as f64,
                lon: lon as f64,
                name,
            });
        }
        Ok(out)
    }

    /// Every peak within `radius_m` of `(lat, lon)`.
    ///
    /// Tile enumeration yields a bounding box, not a disc, so the great-circle filter at
    /// the end is load-bearing: it is what used to be Overpass's `around:` clause, and
    /// without it every query would silently return the corners of the box too.
    pub fn peaks_in_radius(
        &self,
        lat: f64,
        lon: f64,
        radius_m: f64,
    ) -> Result<Vec<Record>, FormatError> {
        let origin = Geodetic::new(lat, lon, 0.0);
        let mut out = Vec::new();

        for (lat0, lon0) in dem::tiles_for_region(lat, lon, radius_m) {
            let Ok(i) = self
                .index
                .binary_search_by(|e| (e.lat0, e.lon0).cmp(&(lat0, lon0)))
            else {
                // No peaks in that tile — ocean, desert, or simply unmapped.
                continue;
            };
            for r in self.tile_records(&self.index[i])? {
                if geo::great_circle_distance(origin, Geodetic::new(r.lat, r.lon, 0.0)) <= radius_m {
                    out.push(r);
                }
            }
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(osm_id: i64, lat: f64, lon: f64, name: &str) -> Record {
        Record {
            osm_id,
            lat,
            lon,
            name: name.to_string(),
        }
    }

    /// Everything within a metre, which is all `f32` coordinates promise.
    fn assert_close(a: &Record, b: &Record) {
        assert_eq!(a.osm_id, b.osm_id);
        assert_eq!(a.name, b.name);
        assert!((a.lat - b.lat).abs() < 1e-5, "lat {} vs {}", a.lat, b.lat);
        assert!((a.lon - b.lon).abs() < 1e-5, "lon {} vs {}", a.lon, b.lon);
    }

    #[test]
    fn round_trips_through_write_and_parse() {
        let records = vec![
            rec(1, 37.65, -118.98, "Mammoth Mountain"),
            rec(2, 46.85, -121.76, "Mount Rainier"),
            rec(3, 37.71, -119.01, "Bloody Mountain"),
        ];
        let file = PeakFile::parse(write(&records)).unwrap();
        assert_eq!(file.len(), 3);

        // Wide enough to catch all three regardless of tile layout.
        let mut got = file.peaks_in_radius(40.0, -120.0, 2_000_000.0).unwrap();
        got.sort_by_key(|r| r.osm_id);
        assert_eq!(got.len(), 3);
        for (a, b) in got.iter().zip(records.iter()) {
            assert_close(a, b);
        }
    }

    #[test]
    fn radius_filter_excludes_peaks_inside_the_tile_box_but_outside_the_circle() {
        // Both land in tiles the enumerator returns for a 20km query, but the second is
        // ~78km away. Without the great-circle filter it would come back too.
        let near = rec(1, 37.70, -118.98, "Near Peak");
        let far = rec(2, 37.65, -119.86, "Far Peak");
        let file = PeakFile::parse(write(&[near.clone(), far.clone()])).unwrap();

        let got = file.peaks_in_radius(37.65214, -118.98018, 20_000.0).unwrap();
        assert_eq!(got.len(), 1, "got {:?}", got);
        assert_eq!(got[0].name, "Near Peak");
    }

    #[test]
    fn peaks_spanning_a_tile_boundary_are_all_found() {
        // Either side of the 38N line, which is a tile edge.
        let records = vec![
            rec(1, 37.999, -119.5, "South Of Line"),
            rec(2, 38.001, -119.5, "North Of Line"),
        ];
        assert_ne!(
            tile_of(records[0].lat, records[0].lon),
            tile_of(records[1].lat, records[1].lon),
            "test is meaningless unless these land in different tiles"
        );

        let file = PeakFile::parse(write(&records)).unwrap();
        let got = file.peaks_in_radius(38.0, -119.5, 5_000.0).unwrap();
        assert_eq!(got.len(), 2, "got {:?}", got);
    }

    #[test]
    fn multibyte_names_survive() {
        let records = vec![
            rec(1, 46.55, 8.0, "Jungfrau"),
            rec(2, 46.56, 8.01, "Dents du Midi"),
            rec(3, 46.57, 8.02, "槍ヶ岳"),
        ];
        let file = PeakFile::parse(write(&records)).unwrap();
        let got = file.peaks_in_radius(46.56, 8.01, 50_000.0).unwrap();
        assert_eq!(got.len(), 3);
        assert!(got.iter().any(|r| r.name == "槍ヶ岳"));
    }

    #[test]
    fn overlong_names_truncate_on_a_char_boundary() {
        // 3 bytes each, so 255/3 = 85 fit exactly and the 86th must not be split.
        let long = "山".repeat(200);
        let file = PeakFile::parse(write(&[rec(1, 46.5, 8.0, &long)])).unwrap();
        let got = file.peaks_in_radius(46.5, 8.0, 10_000.0).unwrap();
        assert_eq!(got[0].name.chars().count(), 85);
        assert_eq!(got[0].name.len(), 255);
    }

    #[test]
    fn empty_input_produces_a_valid_empty_file() {
        let file = PeakFile::parse(write(&[])).unwrap();
        assert!(file.is_empty());
        assert_eq!(file.len(), 0);
        assert!(file.peaks_in_radius(37.0, -118.0, 50_000.0).unwrap().is_empty());
    }

    #[test]
    fn rejects_junk_and_truncation() {
        assert!(matches!(
            PeakFile::parse(b"NOPE____________".to_vec()),
            Err(FormatError::BadMagic)
        ));
        assert!(matches!(
            PeakFile::parse(b"MV".to_vec()),
            Err(FormatError::Truncated(_))
        ));

        let mut wrong_version = write(&[rec(1, 46.5, 8.0, "X")]);
        wrong_version[4] = 99;
        assert!(matches!(
            PeakFile::parse(wrong_version),
            Err(FormatError::Version(99))
        ));

        // Index promises a tile whose records were cut off.
        let full = write(&[rec(1, 46.5, 8.0, "Jungfrau")]);
        assert!(matches!(
            PeakFile::parse(full[..full.len() - 4].to_vec())
                .unwrap()
                .peaks_in_radius(46.5, 8.0, 10_000.0),
            Err(FormatError::Truncated(_))
        ));
    }

    #[test]
    fn tile_of_matches_the_dem_convention() {
        // dem::tiles_for_region treats the tile with south edge lat0 as covering
        // (lat0, lat0 + 1], so a peak must resolve to a tile that enumerator returns.
        for (lat, lon) in [(37.65, -118.98), (46.0, 8.0), (-33.5, 151.2), (0.5, -0.5)] {
            let t = tile_of(lat, lon);
            assert!(
                dem::tiles_for_region(lat, lon, 1.0).contains(&t),
                "tile_of({lat}, {lon}) = {t:?} not in tiles_for_region"
            );
        }
    }
}
