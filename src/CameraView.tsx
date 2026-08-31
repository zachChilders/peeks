import { useEffect, useRef, useState } from "react";
import { getCurrentPosition } from "@tauri-apps/plugin-geolocation";
import {
  startCamera,
  stopCamera,
  startHeadingUpdates,
  stopHeadingUpdates,
  startMotionUpdates,
  stopMotionUpdates,
  startIntrinsicsUpdates,
  stopIntrinsicsUpdates,
  capturePhoto,
  type HeadingReading,
  type MotionReading,
  type CameraIntrinsicsReading,
} from "tauri-plugin-camera-api";
import {
  commands,
  type CalibrationStatus,
  type CameraIntrinsics,
  type CameraPose,
  type Geodetic,
  type PeakWithMetrics,
  type PlacedLabel,
} from "./bindings";
import { fetchElevation } from "./lib/elevation";
import "./CameraView.css";

const CARDINALS = [
  "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE",
  "S", "SSW", "SW", "WSW", "W", "WNW", "NW", "NNW",
];

function cardinal(deg: number): string {
  return CARDINALS[Math.round(deg / 22.5) % 16];
}

/** Splits the horizon's screen points into separate polyline segments wherever two
 * consecutive points (sorted by azimuth, not screen position) land far apart on screen —
 * e.g. the 358°→0° wraparound, or a gap where a ray had no DEM coverage — so those don't
 * draw as a stray line sweeping across the frame. */
function splitHorizonSegments(points: [number, number][], maxGapPx: number): [number, number][][] {
  const segments: [number, number][][] = [];
  let current: [number, number][] = [];
  for (const point of points) {
    const prev = current[current.length - 1];
    if (prev && Math.hypot(point[0] - prev[0], point[1] - prev[1]) > maxGapPx) {
      segments.push(current);
      current = [];
    }
    current.push(point);
  }
  if (current.length > 0) segments.push(current);
  return segments;
}

const EYE_HEIGHT_M = 1.6;
const PEAK_RADIUS_M = 100_000;
// How far out the debug DEM-horizon skyline is swept. Deliberately smaller than
// PEAK_RADIUS_M: it's a visual sanity check against the nearby terrain in frame, not a
// claim about the full peak-fetch radius, and a smaller sweep is cheaper.
const HORIZON_RANGE_M = 30_000;
const DEBUG_LOG_LINES = 12;
// Fallback on-screen horizontal FOV, used only for the few ticks before the first
// intrinsics reading arrives from the camera plugin. It is a poor stand-in — the real
// value on a portrait phone is closer to 35 deg once the resizeAspectFill crop is
// accounted for (see CameraIntrinsics in peakcore's projection.rs) — so anything that
// depends on accurate placement should wait for real intrinsics rather than trust this.
const FALLBACK_HFOV_DEG = 63;
const PROJECTION_INTERVAL_MS = 100;
const LABEL_FONT = "15px -apple-system, BlinkMacSystemFont, sans-serif";
// CLHeading's own confidence, in degrees; negative means CoreLocation couldn't compute a
// heading at all. Rejecting anything worse than this stops the app from confidently
// drawing peaks at a heading that's flat-out wrong -- the usual cause is magnetic
// interference (a parked car, a garage door) right after the compass starts, and it can
// be off by 90+ degrees in that state. Set a bit above the skyline fitter's own +/-20 deg
// yaw search range (peakcore::skyline::FitConfig): a heading this func accepts should be
// close enough that the fitter could still refine it, not so far off that nothing could.
const MAX_HEADING_ACCURACY_DEG = 30;

/** Drops the plugin reading's `timestamp` to get the shape the projection expects. */
function toCameraIntrinsics(reading: CameraIntrinsicsReading): CameraIntrinsics {
  return {
    fovDeg: reading.fovDeg,
    zoomFactor: reading.zoomFactor,
    bufferLongPx: reading.bufferLongPx,
    bufferShortPx: reading.bufferShortPx,
  };
}

/** Pixel `(width, height)` of `text` in the AR label font, via an offscreen canvas —
 * canvas text measurement is a browser API with no Rust equivalent, which is why this
 * one piece of the layout pipeline stays in TypeScript. */
function measureText(ctx: CanvasRenderingContext2D | null, text: string): [number, number] {
  if (!ctx) return [text.length * 8, 18];
  ctx.font = LABEL_FONT;
  return [ctx.measureText(text).width, 18];
}

export default function CameraView({ onClose }: { onClose: () => void }) {
  const [heading, setHeading] = useState<HeadingReading | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [placedLabels, setPlacedLabels] = useState<PlacedLabel[]>([]);
  const [horizonPoints, setHorizonPoints] = useState<[number, number][]>([]);
  const [debugLog, setDebugLog] = useState<string[]>([]);
  const [capturing, setCapturing] = useState(false);
  const [captureFlash, setCaptureFlash] = useState(false);
  const [calibration, setCalibration] = useState<CalibrationStatus | null>(null);

  // Sensor readings arrive far faster than we want to re-layout labels; a periodic
  // interval reads these refs instead of re-rendering on every single event.
  const headingRef = useRef<HeadingReading | null>(null);
  const motionRef = useRef<MotionReading | null>(null);
  const intrinsicsRef = useRef<CameraIntrinsics | null>(null);
  const sceneReadyRef = useRef(false);
  // Whether the most recent heading reading was rejected for low accuracy, so the log
  // line below fires once per transition instead of once per reading (headings arrive
  // many times a second, and a sustained bad fix would otherwise flood the debug HUD's
  // 12-line buffer and push everything else off screen).
  const headingRejectedRef = useRef(false);
  const measureCtxRef = useRef<CanvasRenderingContext2D | null>(null);

  // Visible on-device pipeline trace: TestFlight builds have no attached debugger, so
  // this is how "why are there no labels" gets diagnosed from a screenshot alone.
  function log(msg: string) {
    console.log(msg);
    setDebugLog((prev) => [...prev.slice(-(DEBUG_LOG_LINES - 1)), msg]);
  }

  async function onCapture() {
    if (capturing) return;
    setCapturing(true);
    try {
      await capturePhoto();
      log("capture: saved to Photos");
      // Brief shutter flash — the only feedback a capture happened, since there's no
      // shutter sound/animation from the native side.
      setCaptureFlash(true);
      setTimeout(() => setCaptureFlash(false), 150);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      log(`ERROR [capturePhoto]: ${msg}`);
      setError(`[capturePhoto] ${msg}`);
    } finally {
      setCapturing(false);
    }
  }

  // Camera preview + compass + device motion lifecycle.
  useEffect(() => {
    let cancelled = false;

    async function start() {
      try {
        await startCamera();
      } catch (e) {
        if (!cancelled) setError(`[startCamera] ${e instanceof Error ? e.message : String(e)}`);
      }

      try {
        await startHeadingUpdates((reading, err) => {
          if (err) {
            setError(`[heading] ${err}`);
            return;
          }
          if (!reading) return;

          if (reading.accuracy < 0 || reading.accuracy > MAX_HEADING_ACCURACY_DEG) {
            if (!headingRejectedRef.current) {
              headingRejectedRef.current = true;
              log(`heading: rejected, accuracy ${reading.accuracy.toFixed(0)}°`);
            }
            // Leave headingRef/heading exactly as they are. If no good reading has ever
            // arrived that keeps rendering "Orienting…"; if one already had, freezing on
            // it beats overwriting with a heading we know is untrustworthy.
            return;
          }

          if (headingRejectedRef.current) {
            headingRejectedRef.current = false;
            log(`heading: locked, accuracy ${reading.accuracy.toFixed(0)}°`);
          }
          headingRef.current = reading;
          setHeading(reading);
        });
      } catch (e) {
        if (!cancelled) {
          setError(`[startHeadingUpdates] ${e instanceof Error ? e.message : String(e)}`);
        }
      }

      try {
        await startMotionUpdates((reading, err) => {
          if (err) {
            setError(`[motion] ${err}`);
            return;
          }
          if (reading) motionRef.current = reading;
        });
      } catch (e) {
        if (!cancelled) {
          setError(`[startMotionUpdates] ${e instanceof Error ? e.message : String(e)}`);
        }
      }

      // Must come after startCamera: intrinsics are read off the active capture device.
      // Arrives once immediately, then again on every zoom change.
      try {
        await startIntrinsicsUpdates((reading, err) => {
          if (err) {
            setError(`[intrinsics] ${err}`);
            return;
          }
          if (reading) intrinsicsRef.current = toCameraIntrinsics(reading);
        });
      } catch (e) {
        if (!cancelled) {
          setError(`[startIntrinsicsUpdates] ${e instanceof Error ? e.message : String(e)}`);
        }
      }

      // Skyline fitting. Frames go straight from the native plugin into Rust and never
      // reach this layer, so there is nothing to receive here — only start and stop.
      // Must follow startCamera: frames come off the running capture device.
      const calib = await commands.startCalibration();
      if (calib.status === "error" && !cancelled) {
        log(`ERROR [startCalibration]: ${calib.error}`);
      }
    }

    start();

    return () => {
      cancelled = true;
      commands.stopCalibration().catch(() => {});
      stopIntrinsicsUpdates().catch(() => {});
      stopMotionUpdates().catch(() => {});
      stopHeadingUpdates().catch(() => {});
      stopCamera().catch(() => {});
    };
  }, []);

  // Observer position + ground elevation, then nearby named peaks filtered down to the
  // ones actually visible (terrain occlusion via filterVisiblePeaks, which raycasts
  // against a local DEM downloaded/cached on first use), then hand the scene to Rust
  // once via setScene.
  //
  // This used to run in two passes — a small disc first so something appeared on screen
  // while the slow 100km Overpass query finished. Peaks now come from a dataset bundled
  // with the app, so the full radius resolves off a local file and the staged load has
  // nothing left to hide. No re-fetch-on-movement threshold in this v0 pass.
  useEffect(() => {
    let cancelled = false;

    async function loadPeaks(observer: Geodetic, radiusM: number) {
      const peaksResult = await commands.fetchPeaks(observer.lat, observer.lon, radiusM);
      if (peaksResult.status === "error") throw new Error(peaksResult.error);
      if (cancelled) return;
      log(`peaks: ${peaksResult.data.length} named peaks (radius ${radiusM / 1000}km)`);

      const visibleResult = await commands.filterVisiblePeaks(observer, peaksResult.data, radiusM);
      if (visibleResult.status === "error") throw new Error(visibleResult.error);
      const peaks = visibleResult.data;
      if (cancelled) return;
      log(`peaks: ${peaks.length}/${peaksResult.data.length} visible after occlusion filter`);

      // Text metrics can only come from the browser (canvas measureText has no Rust
      // equivalent), so peak names are measured once, here, and shipped to Rust with
      // the scene rather than re-measured on every 100ms tick. Must wait for the real
      // font to be loaded first — measuring against a fallback font before
      // -apple-system resolves would cache wrong widths for the session.
      await document.fonts.ready;
      if (cancelled) return;
      if (!measureCtxRef.current) {
        const canvas = document.createElement("canvas");
        measureCtxRef.current = canvas.getContext("2d");
      }
      const ctx = measureCtxRef.current;
      const metrics: PeakWithMetrics[] = peaks.map((p) => {
        const [textW, textH] = measureText(ctx, p.name);
        return {
          osmId: p.osmId,
          name: p.name,
          geo: { lat: p.lat, lon: p.lon, alt: p.elev },
          textW,
          textH,
        };
      });

      await commands.setScene(observer, metrics);
      if (!cancelled) sceneReadyRef.current = true;
      log(`peaks: scene set (${peaks.length} peaks)`);
    }

    async function loadHorizon(observer: Geodetic) {
      const result = await commands.computeHorizon(observer, HORIZON_RANGE_M);
      if (result.status === "error") throw new Error(result.error);
      if (cancelled) return;
      log(`horizon: ${result.data.length} points computed (range ${HORIZON_RANGE_M / 1000}km)`);

      await commands.setHorizon(result.data);
    }

    async function start() {
      let step = "getCurrentPosition";
      try {
        const pos = await getCurrentPosition();
        log(`position: ${pos.coords.latitude.toFixed(5)}, ${pos.coords.longitude.toFixed(5)}`);
        step = "fetchGroundElevation";
        const groundElev = await fetchElevation(pos.coords.latitude, pos.coords.longitude);
        if (cancelled) return;
        log(`ground elevation: ${groundElev.toFixed(0)}m`);
        const observer: Geodetic = {
          lat: pos.coords.latitude,
          lon: pos.coords.longitude,
          alt: groundElev + EYE_HEIGHT_M,
        };

        step = "loadPeaks";
        await loadPeaks(observer, PEAK_RADIUS_M);

        step = "loadHorizon";
        await loadHorizon(observer);
      } catch (e) {
        if (!cancelled) {
          const msg = e instanceof Error ? e.message : String(e);
          log(`ERROR [${step}]: ${msg}`);
          setError(`[${step}] ${msg}`);
        }
      }
    }

    start();
    return () => {
      cancelled = true;
    };
  }, []);

  // Re-project + re-layout on a fixed cadence, decoupled from sensor arrival rate. Each
  // tick is one IPC round trip to Rust's project_labels (basis + a handful of dot
  // products per peak — see scene.rs's project_compute_cost for the compute-side
  // measurement); `inFlight` skips a tick rather than piling up calls if one is slow.
  useEffect(() => {
    let inFlight = false;
    // Last effective FOV written to the debug log, so only real changes get a line.
    // -Infinity rather than NaN: every NaN comparison is false, which would suppress the
    // first line entirely — the one that matters most.
    let loggedHfov = Number.NEGATIVE_INFINITY;
    let loggedCalibration = "";

    const id = setInterval(() => {
      if (inFlight || !sceneReadyRef.current) return;
      const h = headingRef.current;
      if (!h) return;

      const yawDeg = h.trueHeading >= 0 ? h.trueHeading : h.magneticHeading;
      const cam: CameraPose = {
        yawDeg,
        pitchDeg: motionRef.current?.pitch ?? 0,
        rollDeg: motionRef.current?.roll ?? 0,
        hfovDeg: FALLBACK_HFOV_DEG,
        width: window.innerWidth,
        height: window.innerHeight,
        // Takes precedence over hfovDeg above; non-null from the first reading onward.
        intrinsics: intrinsicsRef.current,
      };

      inFlight = true;
      // TODO(measured-in-sandbox-only): scene.rs's project_compute_cost measured the
      // Rust-side compute at ~24-162us/call, well under the plan's ~5ms fallback
      // threshold, but that excludes IPC/JSON overhead — this environment has no
      // display server to run the real WebView. Wrap this call in performance.now() on
      // a device before trusting that the full round trip is still comfortably fast.
      commands
        .projectLabels(cam)
        .then(({ labels, horizon, effectiveHfovDeg, calibration }) => {
          setPlacedLabels(labels);
          setHorizonPoints(horizon.map(([x, y]) => [x ?? 0, y ?? 0]));

          // Logged from here rather than the intrinsics callback so the derived FOV comes
          // straight from the projection that used it — no reimplementing the aspect-fill
          // math in TS just to print it. Thresholded so pinching doesn't flood the log.
          const hfov = effectiveHfovDeg ?? 0;
          if (Math.abs(hfov - loggedHfov) > 0.5) {
            loggedHfov = hfov;
            const i = intrinsicsRef.current;
            const src = i
              ? `fov ${i.fovDeg!.toFixed(1)}° zoom ${i.zoomFactor!.toFixed(1)}x`
              : "no intrinsics (fallback)";
            log(`camera: ${src} -> hfov ${hfov.toFixed(1)}°`);
          }

          // The fitter runs entirely in Rust off the native frame stream, so this line is
          // the only visibility into whether it is working. `detail` says which gate
          // rejected a frame rather than just going quiet.
          setCalibration(calibration);
          if (calibration.detail !== loggedCalibration) {
            loggedCalibration = calibration.detail;
            log(`fit: ${calibration.detail}`);
          }
        })
        .catch((e) => setError(`[projectLabels] ${e instanceof Error ? e.message : String(e)}`))
        .finally(() => {
          inFlight = false;
        });
    }, PROJECTION_INTERVAL_MS);

    return () => clearInterval(id);
  }, []);

  // trueHeading is negative when invalid; fall back to magnetic heading.
  const degrees = heading
    ? heading.trueHeading >= 0
      ? heading.trueHeading
      : heading.magneticHeading
    : null;

  const horizonSegments = splitHorizonSegments(horizonPoints, Math.max(window.innerWidth, window.innerHeight));

  return (
    <div className="camera-view">
      <div className="camera-overlay">
        {error && <div className="camera-overlay-error">{error}</div>}
        {degrees !== null ? (
          <div className="camera-heading">
            {degrees.toFixed(0)}&deg; {cardinal(degrees)}
          </div>
        ) : (
          !error && <div className="camera-heading">Orienting&hellip;</div>
        )}
      </div>

      {/* Debug DEM-horizon skyline: the terrain angle the occlusion check computed in
          every direction, projected through the same camera pose as the peak dots. A
          peak dot sitting below this line is a peak the occlusion filter should already
          be dropping; one above it that's still missing points at a different bug. */}
      <svg className="ar-horizon">
        {horizonSegments.map((segment, i) => (
          <polyline key={i} points={segment.map(([x, y]) => `${x},${y}`).join(" ")} />
        ))}
      </svg>

      {/* Generated bindings type every f64 field `number | null` (serde_json encodes
          NaN/Infinity as null), which none of these ever are in practice — the `!`s
          below just opt back into plain-number arithmetic. */}
      <div className="ar-labels">
        {placedLabels.map((label) => (
          <div key={label.osmId}>
            <div
              className="ar-dot"
              style={{ left: label.anchor[0]!, top: label.anchor[1]! }}
            />
            {label.rect && (
              <>
                <svg className="ar-leader">
                  <line
                    x1={label.rect.x! + label.rect.w! / 2}
                    y1={label.rect.y! + label.rect.h!}
                    x2={label.anchor[0]!}
                    y2={label.anchor[1]!}
                  />
                </svg>
                <div
                  className="ar-label"
                  style={{ left: label.rect.x!, top: label.rect.y! }}
                >
                  {label.name}
                </div>
              </>
            )}
          </div>
        ))}
      </div>

      {/* Whether the skyline fitter has a lock, and what it is applying. The overlay
          silently shifting is otherwise indistinguishable from a compass that drifted. */}
      {calibration?.locked && (
        <div className="camera-calibration">
          fit {calibration.dYawDeg!.toFixed(1)}&deg; / {calibration.dPitchDeg!.toFixed(1)}&deg;
          <span className="camera-calibration-rate">
            {" "}
            {calibration.accepted}/{calibration.frames}
          </span>
        </div>
      )}

      <div className="camera-debug-log">{debugLog.join("\n")}</div>

      {/* Peak names and positions come from the bundled OSM extract, which is ODbL — so
          shipping it in the app is redistribution and this notice is a licence
          obligation, not decoration. MapView carries the equivalent for its tile layer. */}
      <div className="camera-attribution">
        Peak data &copy; OpenStreetMap contributors, ODbL
      </div>

      <button
        type="button"
        className="camera-close-button"
        onClick={onClose}
        aria-label="Close camera"
      >
        <svg viewBox="0 0 24 24" width="22" height="22">
          <path
            stroke="currentColor"
            strokeWidth="2.5"
            strokeLinecap="round"
            d="M5 5l14 14M19 5L5 19"
          />
        </svg>
      </button>

      <button
        type="button"
        className="camera-capture-button"
        onClick={onCapture}
        disabled={capturing}
        aria-label="Capture photo"
      >
        <span className="camera-capture-button-inner" />
      </button>

      {captureFlash && <div className="camera-capture-flash" />}
    </div>
  );
}
