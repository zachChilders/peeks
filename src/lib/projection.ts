// Camera pose, projection, and label layout.
//
// Direct port of peaklab/src/projection.rs — see that file for the derivations. Runs
// every frame here since it's cheap arithmetic; no native code needed.

import type { Enu } from "./geo";

export interface CameraPose {
  /** True-north azimuth, degrees, clockwise. */
  yawDeg: number;
  /** Degrees, up positive. */
  pitchDeg: number;
  /** Degrees, clockwise looking along the forward axis. */
  rollDeg: number;
  hfovDeg: number;
  width: number;
  height: number;
}

type Vec3 = [number, number, number];

function dot(a: Vec3, b: Vec3): number {
  return a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
}
function cross(a: Vec3, b: Vec3): Vec3 {
  return [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
}
function norm(a: Vec3): Vec3 {
  const n = Math.sqrt(dot(a, a));
  return [a[0] / n, a[1] / n, a[2] / n];
}
function rotateAbout(v: Vec3, k: Vec3, angleRad: number): Vec3 {
  const s = Math.sin(angleRad);
  const c = Math.cos(angleRad);
  const kxv = cross(k, v);
  const kdv = dot(k, v);
  return [
    v[0] * c + kxv[0] * s + k[0] * kdv * (1 - c),
    v[1] * c + kxv[1] * s + k[1] * kdv * (1 - c),
    v[2] * c + kxv[2] * s + k[2] * kdv * (1 - c),
  ];
}
const toRad = (deg: number) => (deg * Math.PI) / 180;

/** Forward/right/up basis vectors in ENU, roll applied about the forward axis. */
export function cameraBasis(cam: CameraPose): { forward: Vec3; right: Vec3; up: Vec3 } {
  const yaw = toRad(cam.yawDeg);
  const pitch = toRad(cam.pitchDeg);

  const forward: Vec3 = [
    Math.sin(yaw) * Math.cos(pitch),
    Math.cos(yaw) * Math.cos(pitch),
    Math.sin(pitch),
  ];
  const right0: Vec3 = [Math.cos(yaw), -Math.sin(yaw), 0];
  const up0 = norm(cross(right0, forward));

  if (cam.rollDeg === 0) {
    return { forward, right: right0, up: up0 };
  }
  const roll = toRad(cam.rollDeg);
  return {
    forward,
    right: rotateAbout(right0, forward, roll),
    up: rotateAbout(up0, forward, roll),
  };
}

export function focalPx(cam: CameraPose): number {
  return cam.width / 2 / Math.tan(toRad(cam.hfovDeg) / 2);
}

/** Project an ENU vector to pixel coordinates (origin top-left, y down). `null` if behind camera. */
export function project(cam: CameraPose, targetEnu: Enu): [number, number] | null {
  const { forward, right, up } = cameraBasis(cam);
  const v: Vec3 = targetEnu;
  const z = dot(v, forward);
  if (z <= 0) return null;

  const f = focalPx(cam);
  const x = cam.width / 2 + (f * dot(v, right)) / z;
  const y = cam.height / 2 - (f * dot(v, up)) / z;
  return [x, y];
}

export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

function overlaps(a: Rect, b: Rect): boolean {
  return a.x < b.x + b.w && a.x + a.w > b.x && a.y < b.y + b.h && a.y + a.h > b.y;
}

export interface PlacedLabel {
  name: string;
  anchor: [number, number];
  rect: Rect | null;
}

/**
 * Greedily place labels nearest-first, skipping any position that overlaps an
 * already-placed label. Stacks upward on collision before giving up.
 *
 * `candidates` must already be sorted nearest-first.
 */
export function layoutLabels(
  candidates: { name: string; anchor: [number, number] }[],
  measure: (text: string) => [number, number],
  maxStack = 6,
  lineGap = 4,
): PlacedLabel[] {
  const placedRects: Rect[] = [];
  const out: PlacedLabel[] = [];

  for (const { name, anchor } of candidates) {
    const [tw, th] = measure(name);
    let chosen: Rect | null = null;

    for (let stack = 0; stack <= maxStack; stack++) {
      const rect: Rect = {
        x: anchor[0] - tw / 2,
        y: anchor[1] - 10 - (th + lineGap) * (stack + 1),
        w: tw,
        h: th,
      };
      if (!placedRects.some((p) => overlaps(p, rect))) {
        chosen = rect;
        break;
      }
    }

    if (chosen) placedRects.push(chosen);
    out.push({ name, anchor, rect: chosen });
  }

  return out;
}
