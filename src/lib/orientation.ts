// Everything the AR overlay needs to know before it can point at anything, started as
// early as possible and kept alive across views.
//
// # Why this is not inside CameraView
//
// Orienting is slow, and none of the slow parts need the camera. A cold start has to fix
// a GPS position, sample the DEM for ground elevation, pull ~100km of peaks and raycast
// them against the terrain, sweep the DEM for a horizon profile, and — separately, on the
// sensors' own schedule — wait for the magnetometer to settle to a usable accuracy and
// for the gyro's relative-yaw integral to start streaming. Only the last stage, fitting
// the observed skyline to that horizon, needs frames from the capture device.
//
// While all of that ran inside CameraView, opening the camera meant staring at
// "Orienting…" through the whole sequence. It now runs from the landing page instead, so
// by the time the camera opens the scene is usually already set in Rust, the compass has
// settled, and the fitter can start on its first frame.
//
// The state lives in module scope rather than a React context because it outlives every
// view that reads it: navigating landing -> camera -> landing must not restart the
// compass, and unmounting CameraView must not discard a scene that took seconds to build.

import { getCurrentPosition, requestPermissions } from "@tauri-apps/plugin-geolocation";
import {
  startHeadingUpdates,
  startMotionUpdates,
  type HeadingReading,
  type MotionReading,
} from "tauri-plugin-camera-api";
import { commands, type Geodetic, type PeakWithMetrics } from "../bindings";
import { fetchElevation } from "./elevation";
import { log } from "./debugLog";

const EYE_HEIGHT_M = 1.6;
const PEAK_RADIUS_M = 100_000;
// How far out the debug DEM-horizon skyline is swept. Deliberately smaller than
// PEAK_RADIUS_M: it's a visual sanity check against the nearby terrain in frame, not a
// claim about the full peak-fetch radius, and a smaller sweep is cheaper.
const HORIZON_RANGE_M = 30_000;
const LABEL_FONT = "15px -apple-system, BlinkMacSystemFont, sans-serif";

// CLHeading's own confidence, in degrees; negative means CoreLocation couldn't compute a
// heading at all. Rejecting anything worse than this stops the app from confidently
// drawing peaks at a heading that's flat-out wrong -- the usual cause is magnetic
// interference (a parked car, a garage door) right after the compass starts, and it can
// be off by 90+ degrees in that state. Set a bit above the skyline fitter's own +/-20 deg
// yaw search range (peakcore::skyline::FitConfig): a heading this func accepts should be
// close enough that the fitter could still refine it, not so far off that nothing could.
//
// That is now the compass's entire job. It has to land the overlay inside the fitter's
// search window; the fitter supplies the absolute answer and the heading is then held on
// the gyro datum without consulting this again (see src-tauri/src/calibration.rs).
const MAX_HEADING_ACCURACY_DEG = 30;

// How far the observer can move from the position the scene was built for before that
// scene is rebuilt. Building early is only a win if what was built is still true: peak
// visibility and the horizon profile are both computed for one point, and a few hundred
// metres of walking (let alone a drive to the trailhead while the app sits on the landing
// page) can put a ridge in front of a summit that was visible from where you started.
// Well under the scale at which either result changes, and far enough above GPS jitter
// that a stationary phone never triggers a rebuild.
const OBSERVER_STALE_M = 250;

export type OrientationStep =
  | "idle"
  | "locating"
  | "peaks"
  | "horizon"
  | "ready"
  | "error";

export type OrientationState = {
  step: OrientationStep;
  /** What the current step is waiting on, for the landing page's progress line. */
  detail: string;
  error: string | null;
  /** Where the scene currently in Rust was built for, eye height included. */
  observer: Geodetic | null;
  peakCount: number | null;
  /** True once `setScene` has landed, i.e. `projectLabels` can return labels. */
  sceneReady: boolean;
  /** A compass reading good enough to point the overlay with has arrived. */
  headingReady: boolean;
  /** The gyro datum the fitted heading is held on is streaming. */
  motionReady: boolean;
};

const INITIAL: OrientationState = {
  step: "idle",
  detail: "not started",
  error: null,
  observer: null,
  peakCount: null,
  sceneReady: false,
  headingReady: false,
  motionReady: false,
};

let state: OrientationState = INITIAL;
const listeners = new Set<() => void>();

function setState(patch: Partial<OrientationState>): void {
  state = { ...state, ...patch };
  for (const listener of listeners) listener();
}

/** Stable reference between changes, as `useSyncExternalStore` requires. */
export function orientationSnapshot(): OrientationState {
  return state;
}

export function subscribeOrientation(onChange: () => void): () => void {
  listeners.add(onChange);
  return () => {
    listeners.delete(onChange);
  };
}

// Sensor readings arrive many times a second — far faster than either the projection tick
// or any sane re-render — so they are held here as plain values and read on demand.
// Only CameraView's heading readout wants one render per reading, and it opts in through
// `subscribeHeading`.
let heading: HeadingReading | null = null;
let motion: MotionReading | null = null;
const headingListeners = new Set<(reading: HeadingReading) => void>();

export function currentHeading(): HeadingReading | null {
  return heading;
}

export function currentMotion(): MotionReading | null {
  return motion;
}

export function subscribeHeading(cb: (reading: HeadingReading) => void): () => void {
  headingListeners.add(cb);
  return () => {
    headingListeners.delete(cb);
  };
}

export function isSceneReady(): boolean {
  return state.sceneReady;
}

/** Pixel `(width, height)` of `text` in the AR label font, via an offscreen canvas —
 * canvas text measurement is a browser API with no Rust equivalent, which is why this
 * one piece of the layout pipeline stays in TypeScript. */
function measureText(ctx: CanvasRenderingContext2D | null, text: string): [number, number] {
  if (!ctx) return [text.length * 8, 18];
  ctx.font = LABEL_FONT;
  return [ctx.measureText(text).width, 18];
}

let measureCtx: CanvasRenderingContext2D | null = null;

/** Great-circle distance in metres. Only used against `OBSERVER_STALE_M`, so the
 * spherical approximation is far more precision than the decision needs.
 *
 * The `!`s are the same opt-out the AR overlay makes: generated bindings type every f64
 * field `number | null` because serde_json encodes NaN/Infinity as null, which a
 * coordinate never is in practice. */
function distanceM(a: Geodetic, b: Geodetic): number {
  const R = 6_371_000;
  const toRad = Math.PI / 180;
  const aLat = a.lat! * toRad;
  const bLat = b.lat! * toRad;
  const dLat = bLat - aLat;
  const dLon = (b.lon! - a.lon!) * toRad;
  const s =
    Math.sin(dLat / 2) ** 2 + Math.cos(aLat) * Math.cos(bLat) * Math.sin(dLon / 2) ** 2;
  return 2 * R * Math.asin(Math.min(1, Math.sqrt(s)));
}

let sensorsStarted = false;

// Compass and gyro, started once and then left running for the life of the app.
//
// Deliberately never stopped: the settling time this exists to hide is paid on *start*,
// so stopping them when CameraView unmounts would hand the cost straight back to the
// next visit to the camera. Neither stream is expensive next to the capture session,
// which is still started and stopped with the view.
async function startSensors(): Promise<void> {
  if (sensorsStarted) return;
  sensorsStarted = true;

  // Whether the most recent heading reading was rejected for low accuracy, so the log
  // line below fires once per transition instead of once per reading (headings arrive
  // many times a second, and a sustained bad fix would otherwise flood the log).
  let headingRejected = false;

  try {
    await startHeadingUpdates((reading, err) => {
      if (err) {
        setState({ error: `[heading] ${err}` });
        return;
      }
      if (!reading) return;

      if (reading.accuracy < 0 || reading.accuracy > MAX_HEADING_ACCURACY_DEG) {
        if (!headingRejected) {
          headingRejected = true;
          log(`heading: rejected, accuracy ${reading.accuracy.toFixed(0)}°`);
        }
        // Leave the held reading exactly as it is. If no good one has ever arrived that
        // keeps rendering "Orienting…"; if one already had, freezing on it beats
        // overwriting with a heading we know is untrustworthy.
        return;
      }

      if (headingRejected) {
        headingRejected = false;
        log(`heading: locked, accuracy ${reading.accuracy.toFixed(0)}°`);
      }
      heading = reading;
      if (!state.headingReady) setState({ headingReady: true });
      for (const listener of headingListeners) listener(reading);
    });
  } catch (e) {
    setState({ error: `[startHeadingUpdates] ${message(e)}` });
  }

  try {
    await startMotionUpdates((reading, err) => {
      if (err) {
        setState({ error: `[motion] ${err}` });
        return;
      }
      if (!reading) return;
      motion = reading;
      if (!state.motionReady) setState({ motionReady: true });
    });
  } catch (e) {
    setState({ error: `[startMotionUpdates] ${message(e)}` });
  }
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

// One scene build at a time. A request that arrives mid-build (the camera asking for a
// rebuild while the landing page's first build is still running) is queued rather than
// run concurrently: two builds racing would have their `setScene`/`setHorizon` calls land
// in whatever order the Rust side finished them, which is how a stale scene ends up
// winning.
let building = false;
let rebuildQueued = false;

async function loadPeaks(observer: Geodetic): Promise<number> {
  const peaksResult = await commands.fetchPeaks(observer.lat, observer.lon, PEAK_RADIUS_M);
  if (peaksResult.status === "error") throw new Error(peaksResult.error);
  log(`peaks: ${peaksResult.data.length} named peaks (radius ${PEAK_RADIUS_M / 1000}km)`);

  const visibleResult = await commands.filterVisiblePeaks(
    observer,
    peaksResult.data,
    PEAK_RADIUS_M,
  );
  if (visibleResult.status === "error") throw new Error(visibleResult.error);
  const peaks = visibleResult.data;
  log(`peaks: ${peaks.length}/${peaksResult.data.length} visible after occlusion filter`);

  // Text metrics can only come from the browser (canvas measureText has no Rust
  // equivalent), so peak names are measured once, here, and shipped to Rust with the
  // scene rather than re-measured on every 100ms tick. Must wait for the real font to be
  // loaded first — measuring against a fallback font before -apple-system resolves would
  // cache wrong widths for the session.
  await document.fonts.ready;
  if (!measureCtx) {
    measureCtx = document.createElement("canvas").getContext("2d");
  }
  const metrics: PeakWithMetrics[] = peaks.map((p) => {
    const [textW, textH] = measureText(measureCtx, p.name);
    return {
      osmId: p.osmId,
      name: p.name,
      geo: { lat: p.lat, lon: p.lon, alt: p.elev },
      textW,
      textH,
    };
  });

  await commands.setScene(observer, metrics);
  log(`peaks: scene set (${peaks.length} peaks)`);
  return peaks.length;
}

async function loadHorizon(observer: Geodetic): Promise<void> {
  const result = await commands.computeHorizon(observer, HORIZON_RANGE_M);
  if (result.status === "error") throw new Error(result.error);
  log(`horizon: ${result.data.length} points computed (range ${HORIZON_RANGE_M / 1000}km)`);
  await commands.setHorizon(result.data);
}

/** Position -> ground elevation -> visible peaks -> horizon profile, handed to Rust.
 *
 * This used to run in two passes — a small disc first so something appeared on screen
 * while the slow 100km Overpass query finished. Peaks now come from a dataset bundled
 * with the app, so the full radius resolves off a local file and the staged load has
 * nothing left to hide. */
async function buildScene(): Promise<void> {
  if (building) {
    rebuildQueued = true;
    return;
  }
  building = true;

  let step = "requestPermissions";
  try {
    setState({ step: "locating", detail: "waiting for a position fix", error: null });
    // The landing page is the first screen now, so this may well be the prompt that asks
    // for location at all; MapView used to be where that happened.
    await requestPermissions(["location"]);

    step = "getCurrentPosition";
    const pos = await getCurrentPosition();
    log(`position: ${pos.coords.latitude.toFixed(5)}, ${pos.coords.longitude.toFixed(5)}`);

    step = "fetchGroundElevation";
    const groundElev = await fetchElevation(pos.coords.latitude, pos.coords.longitude);
    log(`ground elevation: ${groundElev.toFixed(0)}m`);
    const observer: Geodetic = {
      lat: pos.coords.latitude,
      lon: pos.coords.longitude,
      alt: groundElev + EYE_HEIGHT_M,
    };

    step = "loadPeaks";
    setState({ step: "peaks", detail: "finding visible peaks", observer });
    const peakCount = await loadPeaks(observer);
    // Labels can be projected from here on; the horizon below is what the fitter needs.
    setState({ peakCount, sceneReady: true });

    step = "loadHorizon";
    setState({ step: "horizon", detail: "sweeping the terrain horizon" });
    await loadHorizon(observer);

    setState({ step: "ready", detail: `${peakCount} peaks in view` });
  } catch (e) {
    // `sceneReady` is deliberately left alone. If this was a rebuild, the scene already in
    // Rust is from a position that has since moved, but stale labels plus the error banner
    // beat an empty overlay with no explanation.
    log(`ERROR [${step}]: ${message(e)}`);
    setState({ step: "error", detail: step, error: `[${step}] ${message(e)}` });
  } finally {
    building = false;
    if (rebuildQueued) {
      rebuildQueued = false;
      void buildScene();
    }
  }
}

/** Start orienting: sensors first (they settle on their own schedule, so the earlier the
 * better), then the scene. Idempotent — every view that depends on orientation calls it,
 * and only the first call does anything. */
export function startOrientation(): void {
  void startSensors();
  if (state.step === "idle") void buildScene();
}

/** Rebuild the scene from scratch, after an error or on the user's say-so. */
export function retryOrientation(): void {
  void startSensors();
  void buildScene();
}

/** Called when the camera opens: make sure what was built early is still true.
 *
 * Rebuilds if the scene never got built, if it failed, or if the observer has moved far
 * enough (`OBSERVER_STALE_M`) that peak visibility and the horizon profile would come out
 * differently. The existing scene stays live throughout — a rebuild swaps it out when it
 * finishes rather than blanking the overlay while it runs. */
export async function refreshOrientation(): Promise<void> {
  void startSensors();
  if (state.step === "idle" || state.step === "error") {
    void buildScene();
    return;
  }
  const built = state.observer;
  if (!built || building) return;

  try {
    const pos = await getCurrentPosition();
    const moved = distanceM(built, {
      lat: pos.coords.latitude,
      lon: pos.coords.longitude,
      alt: built.alt,
    });
    if (moved > OBSERVER_STALE_M) {
      log(`orientation: moved ${moved.toFixed(0)}m since the scene was built, rebuilding`);
      void buildScene();
    }
  } catch (e) {
    // A failed freshness check is not a failed orientation: the scene already in Rust is
    // still the best thing available, so say so and keep using it.
    log(`orientation: staleness check failed (${message(e)}), keeping the current scene`);
  }
}
