// WGS84 geodesy: ECEF/ENU transforms, look angles, great-circle helpers.
//
// Direct port of peaklab/src/geo.rs (same formulas, same variable names where possible)
// so the desktop tool's validated math and this mobile copy stay easy to cross-check by
// eye. See that file for the derivations/rationale; this file just re-states them in TS.

const WGS84_A = 6_378_137.0;
const WGS84_F = 1.0 / 298.257_223_563;
const WGS84_E2 = WGS84_F * (2.0 - WGS84_F);

/** Mean Earth radius, used only for great-circle distance and the refraction term. */
export const EARTH_MEAN_R = 6_371_008.8;

/** Effective-radius coefficient for standard atmospheric refraction. */
const REFRACTION_K = 7.0 / 6.0;

export interface Geodetic {
  /** Degrees, north positive. */
  lat: number;
  /** Degrees, east positive. */
  lon: number;
  /** Metres above the ellipsoid. */
  alt: number;
}

export type Enu = [east: number, north: number, up: number];

function toRad(deg: number): number {
  return (deg * Math.PI) / 180;
}
function toDeg(rad: number): number {
  return (rad * 180) / Math.PI;
}

function toEcef(p: Geodetic): [number, number, number] {
  const sinLat = Math.sin(toRad(p.lat));
  const cosLat = Math.cos(toRad(p.lat));
  const sinLon = Math.sin(toRad(p.lon));
  const cosLon = Math.cos(toRad(p.lon));
  const n = WGS84_A / Math.sqrt(1.0 - WGS84_E2 * sinLat * sinLat);
  return [
    (n + p.alt) * cosLat * cosLon,
    (n + p.alt) * cosLat * sinLon,
    (n * (1.0 - WGS84_E2) + p.alt) * sinLat,
  ];
}

/** Vector from `observer` to `target` in the observer's local East/North/Up frame. */
export function enu(observer: Geodetic, target: Geodetic): Enu {
  const o = toEcef(observer);
  const t = toEcef(target);
  const d: [number, number, number] = [t[0] - o[0], t[1] - o[1], t[2] - o[2]];

  const sinLat = Math.sin(toRad(observer.lat));
  const cosLat = Math.cos(toRad(observer.lat));
  const sinLon = Math.sin(toRad(observer.lon));
  const cosLon = Math.cos(toRad(observer.lon));

  return [
    -sinLon * d[0] + cosLon * d[1],
    -sinLat * cosLon * d[0] - sinLat * sinLon * d[1] + cosLat * d[2],
    cosLat * cosLon * d[0] + cosLat * sinLon * d[1] + sinLat * d[2],
  ];
}

/** Azimuth of an ENU vector: degrees clockwise from true north, in `[0, 360)`. */
export function azimuthDeg([e, n]: Enu): number {
  const a = toDeg(Math.atan2(e, n));
  return a < 0 ? a + 360 : a;
}

/** Geometric elevation angle of an ENU vector, in degrees (up positive). */
export function elevationDeg([e, n, u]: Enu): number {
  return toDeg(Math.atan2(u, Math.hypot(e, n)));
}

export function horizontalRange([e, n]: Enu): number {
  return Math.hypot(e, n);
}

/** Apparent upward lift from standard atmospheric refraction, in degrees. */
export function refractionLiftDeg(distanceM: number): number {
  return toDeg((distanceM * (1.0 - 1.0 / REFRACTION_K)) / (2.0 * EARTH_MEAN_R));
}

/** Look angles from observer to target: `{ azimuthDeg, elevationDeg, rangeM }`. */
export function lookAngles(
  observer: Geodetic,
  target: Geodetic,
): { azimuthDeg: number; elevationDeg: number; rangeM: number } {
  const v = enu(observer, target);
  const rangeM = Math.hypot(v[0], v[1], v[2]);
  const elev = elevationDeg(v) + refractionLiftDeg(horizontalRange(v));
  return { azimuthDeg: azimuthDeg(v), elevationDeg: elev, rangeM };
}

/** Great-circle (surface) distance in metres. */
export function greatCircleDistance(a: Geodetic, b: Geodetic): number {
  const lat1 = toRad(a.lat);
  const lat2 = toRad(b.lat);
  const dlat = lat2 - lat1;
  const dlon = toRad(b.lon - a.lon);
  const h =
    Math.sin(dlat / 2) ** 2 + Math.cos(lat1) * Math.cos(lat2) * Math.sin(dlon / 2) ** 2;
  return 2.0 * EARTH_MEAN_R * Math.asin(Math.sqrt(h));
}

/** Smallest signed difference `a - b` between two azimuths, in `(-180, 180]`. */
export function angleDiffDeg(a: number, b: number): number {
  let d = (a - b) % 360;
  if (d > 180) d -= 360;
  else if (d <= -180) d += 360;
  return d;
}
