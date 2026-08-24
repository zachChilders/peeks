// Named peaks from OpenStreetMap, elevations from Open-Elevation (SRTM-backed).
//
// Mobile v0 scope cut vs. peaklab (the desktop tool): no local DEM, so no snap-to-local-
// maximum and no visibility/occlusion filtering — peaks behind a nearer ridge will still
// show. peaklab's own M2 measurement found OSM node placement is already accurate
// (~11m median vs. DEM, see plugins/../peaklab/src/peaks.rs) with *zero* snapping, so
// skipping that step here isn't a meaningful accuracy loss on its own; the missing
// occlusion filter is the real known gap versus the desktop pipeline.

import { invoke } from "@tauri-apps/api/core";
import type { Geodetic } from "./geo";

const ELEVATION_URL = "https://api.open-elevation.com/api/v1/lookup";
const ELEVATION_BATCH_SIZE = 100;

export interface Peak {
  name: string;
  osmId: number;
  lat: number;
  lon: number;
  /** Open-Elevation (SRTM-backed) elevation in metres. */
  elev: number;
}

interface OverpassNode {
  id: number;
  lat: number;
  lon: number;
  tags?: Record<string, string>;
}

/** Fetch named peaks within `radiusM` of a point. */
export async function fetchPeaks(
  lat: number,
  lon: number,
  radiusM: number,
): Promise<Peak[]> {
  // Routed through a Rust command (src-tauri/src/overpass.rs), not a webview fetch():
  // Overpass requires a descriptive User-Agent header (HTTP 406 otherwise), and browser
  // fetch() can never set that header — it's forbidden by the Fetch spec and silently
  // stripped, so this genuinely can't be done from JS. reqwest can set it fine.
  const text = await invoke<string>("fetch_peaks_overpass", { lat, lon, radiusM });
  const body = JSON.parse(text);
  const nodes = (body?.elements ?? []) as OverpassNode[];

  const named = nodes.filter((n) => n.tags?.name);
  const elevations = await fetchElevations(named.map((n) => [n.lat, n.lon]));

  return named.map((n, i) => ({
    name: n.tags!.name,
    osmId: n.id,
    lat: n.lat,
    lon: n.lon,
    elev: elevations[i],
  }));
}

/** Batched Open-Elevation lookup, matching the input order. */
async function fetchElevations(points: [number, number][]): Promise<number[]> {
  const out: number[] = new Array(points.length).fill(0);

  for (let start = 0; start < points.length; start += ELEVATION_BATCH_SIZE) {
    const batch = points.slice(start, start + ELEVATION_BATCH_SIZE);
    const res = await fetch(ELEVATION_URL, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        locations: batch.map(([latitude, longitude]) => ({ latitude, longitude })),
      }),
    });
    if (!res.ok) throw new Error(`Open-Elevation returned ${res.status}`);
    const body = await res.json();
    const results = (body?.results ?? []) as { elevation: number }[];
    results.forEach((r, i) => {
      out[start + i] = r.elevation;
    });
  }

  return out;
}

export function peakToGeodetic(p: Peak): Geodetic {
  return { lat: p.lat, lon: p.lon, alt: p.elev };
}
