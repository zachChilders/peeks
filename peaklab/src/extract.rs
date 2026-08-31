//! One-time bulk extract of every named OSM peak into the bundled dataset.
//!
//! The app used to query Overpass on every launch. This walks the globe once, in
//! adaptively-subdivided bounding boxes, and writes a [`peakcore::peakfile`] the app
//! reads from disk instead — trading ~690k peaks fetched once here for two queries per
//! launch per user forever, which is a large net *reduction* in load on Overpass.
//!
//! Each cell's raw response is cached as JSON, mirroring [`crate::peaks::fetch_raw`], so
//! a run interrupted 25 minutes in resumes rather than starting over.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use peakcore::overpass::{self, RawPeak};
use peakcore::peakfile::{self, Record};

const OVERPASS_URL: &str = "https://overpass-api.de/api/interpreter";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(360);
const USER_AGENT: &str = "peaklab/0.1 (one-time peak dataset extract; contact via repo)";

/// Starting cell size. Most of the globe is ocean and answers instantly; the few dense
/// cells subdivide themselves.
const INITIAL_CELL_DEG: f64 = 20.0;
/// A cell this small that still fails is a real error, not a size problem.
const MIN_CELL_DEG: f64 = 1.25;
/// Overpass allows 2 concurrent slots per IP; one sequential query with a pause between
/// is well inside that.
const POLITE_DELAY: Duration = Duration::from_secs(2);
const MAX_RETRIES: usize = 3;
const RETRY_BACKOFF: Duration = Duration::from_secs(30);

pub fn cache_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("extract")
}

fn cell_cache_path(cache_dir: &Path, s: f64, w: f64, n: f64, e: f64) -> PathBuf {
    cache_dir.join(format!("cell_{s:.4}_{w:.4}_{n:.4}_{e:.4}.json"))
}

/// One cell, with retries. `Ok(None)` means Overpass refused in a way that suggests the
/// cell is too big — the caller should subdivide rather than give up.
fn fetch_cell(
    client: &reqwest::blocking::Client,
    s: f64,
    w: f64,
    n: f64,
    e: f64,
) -> Result<Option<Vec<RawPeak>>> {
    let query = overpass::build_bbox_query(s, w, n, e);

    for attempt in 1..=MAX_RETRIES {
        let resp = client
            .post(OVERPASS_URL)
            .form(&[("data", query.as_str())])
            .send();

        match resp {
            Ok(r) if r.status().is_success() => {
                let body = r.text().context("reading Overpass body")?;
                return Ok(Some(overpass::parse_response(&body)?));
            }
            // 429 = our rate limit, 504 = their dispatcher gave up. Both are worth
            // waiting out; a 504 that survives every retry means the cell is too big.
            Ok(r) => {
                let status = r.status();
                if attempt == MAX_RETRIES {
                    if status.as_u16() == 504 || status.as_u16() == 429 {
                        return Ok(None);
                    }
                    bail!("Overpass returned {status} for ({s},{w},{n},{e})");
                }
                eprintln!("    {status}, retry {attempt}/{MAX_RETRIES} after backoff");
                std::thread::sleep(RETRY_BACKOFF);
            }
            Err(err) => {
                if attempt == MAX_RETRIES {
                    // A client-side timeout on a huge cell is the same signal as a 504.
                    if err.is_timeout() {
                        return Ok(None);
                    }
                    return Err(err).context("Overpass request failed");
                }
                eprintln!("    {err}, retry {attempt}/{MAX_RETRIES} after backoff");
                std::thread::sleep(RETRY_BACKOFF);
            }
        }
    }
    unreachable!("loop returns or bails on the final attempt")
}

/// Fetch a cell, subdividing into quadrants if Overpass can't handle it whole.
fn collect_cell(
    client: &reqwest::blocking::Client,
    cache_dir: &Path,
    s: f64,
    w: f64,
    n: f64,
    e: f64,
    out: &mut HashMap<i64, RawPeak>,
) -> Result<()> {
    let path = cell_cache_path(cache_dir, s, w, n, e);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let cached: Vec<RawPeak> =
            serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
        println!("  ({s:>6.1},{w:>7.1})..({n:>6.1},{e:>7.1})  {:>6} cached", cached.len());
        out.extend(cached.into_iter().map(|p| (p.osm_id, p)));
        return Ok(());
    }

    std::thread::sleep(POLITE_DELAY);
    let started = std::time::Instant::now();
    match fetch_cell(client, s, w, n, e)? {
        Some(peaks) => {
            println!(
                "  ({s:>6.1},{w:>7.1})..({n:>6.1},{e:>7.1})  {:>6} peaks  {:.0}s",
                peaks.len(),
                started.elapsed().as_secs_f64()
            );
            std::fs::write(&path, serde_json::to_string(&peaks)?)
                .with_context(|| format!("writing {}", path.display()))?;
            out.extend(peaks.into_iter().map(|p| (p.osm_id, p)));
            Ok(())
        }
        None => {
            let (dlat, dlon) = (n - s, e - w);
            if dlat <= MIN_CELL_DEG && dlon <= MIN_CELL_DEG {
                bail!("Overpass keeps refusing a {dlat}°x{dlon}° cell at ({s},{w}) — not a size problem");
            }
            println!("  ({s:>6.1},{w:>7.1})..({n:>6.1},{e:>7.1})  too big, subdividing");
            let (ms, me) = (s + dlat / 2.0, w + dlon / 2.0);
            collect_cell(client, cache_dir, s, w, ms, me, out)?;
            collect_cell(client, cache_dir, s, me, ms, e, out)?;
            collect_cell(client, cache_dir, ms, w, n, me, out)?;
            collect_cell(client, cache_dir, ms, me, n, e, out)?;
            Ok(())
        }
    }
}

/// Round a bound outward to the [`INITIAL_CELL_DEG`] grid.
///
/// Keeping every run on one global grid is what makes a scoped extract reusable: the
/// cells a `--bbox` run fetches are byte-identical to the ones a later global run wants,
/// so the cache carries straight over instead of being thrown away.
fn snap_out(v: f64, up: bool, limit: f64) -> f64 {
    let cells = v / INITIAL_CELL_DEG;
    let snapped = if up { cells.ceil() } else { cells.floor() };
    // Clamped to the axis' own limit: snapping 84°N outward lands on 100, which is not a
    // latitude. The final cell in a row is then shorter than INITIAL_CELL_DEG, which is
    // fine — cell edges still fall on the global grid.
    (snapped * INITIAL_CELL_DEG).clamp(-limit, limit)
}

/// Walk a region (or the whole globe, when `bounds` is `None`) and write the peak file.
///
/// `bounds` is `(south, west, north, east)` in degrees, snapped outward to the cell grid.
pub fn extract_all(
    data_dir: &Path,
    out_path: &Path,
    bounds: Option<(f64, f64, f64, f64)>,
) -> Result<()> {
    let (south, west, north, east) = match bounds {
        Some((s, w, n, e)) => (
            snap_out(s, false, 90.0),
            snap_out(w, false, 180.0),
            snap_out(n, true, 90.0),
            snap_out(e, true, 180.0),
        ),
        None => (-90.0, -180.0, 90.0, 180.0),
    };
    if south >= north || west >= east {
        bail!("empty region after snapping: ({south},{west})..({north},{east})");
    }
    println!("region ({south},{west})..({north},{east}), {INITIAL_CELL_DEG}° cells");

    let cache = cache_dir(data_dir);
    std::fs::create_dir_all(&cache)
        .with_context(|| format!("creating {}", cache.display()))?;

    let client = reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        // Overpass rejects requests without a descriptive User-Agent (HTTP 406).
        .user_agent(USER_AGENT)
        .build()?;

    // Deduped by OSM id: bounding boxes are inclusive on their edges, so a node sitting
    // exactly on a cell boundary comes back from both neighbours.
    let mut all: HashMap<i64, RawPeak> = HashMap::new();
    let started = std::time::Instant::now();

    // `ceil`, with each cell's far edge clamped to the region: a region whose span isn't
    // a whole number of cells (0..90 latitude) needs a final short row rather than one
    // that overshoots into invalid coordinates.
    let lat_steps = ((north - south) / INITIAL_CELL_DEG).ceil() as i32;
    let lon_steps = ((east - west) / INITIAL_CELL_DEG).ceil() as i32;
    let total = lat_steps * lon_steps;
    println!("{total} cells");
    let mut done = 0;

    for i in 0..lat_steps {
        let s = south + f64::from(i) * INITIAL_CELL_DEG;
        let n = (s + INITIAL_CELL_DEG).min(north);
        for j in 0..lon_steps {
            let w = west + f64::from(j) * INITIAL_CELL_DEG;
            let e = (w + INITIAL_CELL_DEG).min(east);
            done += 1;
            print!("[{done}/{total}] ");
            collect_cell(&client, &cache, s, w, n, e, &mut all)?;
        }
    }

    let records: Vec<Record> = all
        .into_values()
        .map(|p| Record {
            osm_id: p.osm_id,
            lat: p.lat,
            lon: p.lon,
            name: p.name,
        })
        .collect();

    let bytes = peakfile::write(&records);
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out_path, &bytes)
        .with_context(|| format!("writing {}", out_path.display()))?;

    println!();
    println!("{} peaks -> {}", records.len(), out_path.display());
    println!(
        "{:.1} MB in {:.0}s",
        bytes.len() as f64 / 1_048_576.0,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
