import { useEffect, useRef, useState } from "react";
import { getCurrentPosition } from "@tauri-apps/plugin-geolocation";
import {
  startCamera,
  stopCamera,
  startHeadingUpdates,
  stopHeadingUpdates,
  startMotionUpdates,
  stopMotionUpdates,
  type HeadingReading,
  type MotionReading,
} from "tauri-plugin-camera-api";
import { commands, type CameraPose, type Geodetic, type PeakWithMetrics, type PlacedLabel } from "./bindings";
import { fetchPeaks } from "./lib/peaks";
import "./CameraView.css";

const CARDINALS = [
  "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE",
  "S", "SSW", "SW", "WSW", "W", "WNW", "NW", "NNW",
];

function cardinal(deg: number): string {
  return CARDINALS[Math.round(deg / 22.5) % 16];
}

const EYE_HEIGHT_M = 1.6;
const PEAK_RADIUS_M = 100_000;
// iPhone wide-camera horizontal FOV is roughly this, but not read from real device
// intrinsics — a rough starting guess in the same spirit as peaklab's manual yaw tuning.
// Expect to calibrate against a real photo/device the same way.
const HFOV_DEG = 63;
const PROJECTION_INTERVAL_MS = 100;
const LABEL_FONT = "15px -apple-system, BlinkMacSystemFont, sans-serif";

async function fetchGroundElevation(lat: number, lon: number): Promise<number> {
  const url = `https://api.open-elevation.com/api/v1/lookup?locations=${lat},${lon}`;
  const res = await fetch(url);
  if (!res.ok) throw new Error(`Elevation lookup failed: ${res.status}`);
  const body = await res.json();
  const result = body?.results?.[0]?.elevation;
  if (typeof result !== "number") throw new Error("Elevation lookup returned no data");
  return result;
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

  // Sensor readings arrive far faster than we want to re-layout labels; a periodic
  // interval reads these refs instead of re-rendering on every single event.
  const headingRef = useRef<HeadingReading | null>(null);
  const motionRef = useRef<MotionReading | null>(null);
  const sceneReadyRef = useRef(false);
  const measureCtxRef = useRef<CanvasRenderingContext2D | null>(null);

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
          if (reading) {
            headingRef.current = reading;
            setHeading(reading);
          }
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
    }

    start();

    return () => {
      cancelled = true;
      stopMotionUpdates().catch(() => {});
      stopHeadingUpdates().catch(() => {});
      stopCamera().catch(() => {});
    };
  }, []);

  // Observer position + ground elevation, then nearby named peaks, then hand the whole
  // scene to Rust once via setScene. Fetched once — no re-fetch-on-movement threshold in
  // this v0 pass (see lib/peaks.ts for the fuller scope-cut rationale: no local DEM on
  // mobile yet, so no visibility filtering either).
  useEffect(() => {
    let cancelled = false;

    async function start() {
      let step = "getCurrentPosition";
      try {
        const pos = await getCurrentPosition();
        step = "fetchGroundElevation";
        const groundElev = await fetchGroundElevation(
          pos.coords.latitude,
          pos.coords.longitude,
        );
        if (cancelled) return;
        const observer: Geodetic = {
          lat: pos.coords.latitude,
          lon: pos.coords.longitude,
          alt: groundElev + EYE_HEIGHT_M,
        };

        step = "fetchPeaks";
        const peaks = await fetchPeaks(
          pos.coords.latitude,
          pos.coords.longitude,
          PEAK_RADIUS_M,
        );
        if (cancelled) return;

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

        step = "setScene";
        await commands.setScene(observer, metrics);
        if (!cancelled) sceneReadyRef.current = true;
      } catch (e) {
        if (!cancelled) {
          setError(`[${step}] ${e instanceof Error ? e.message : String(e)}`);
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

    const id = setInterval(() => {
      if (inFlight || !sceneReadyRef.current) return;
      const h = headingRef.current;
      if (!h) return;

      const yawDeg = h.trueHeading >= 0 ? h.trueHeading : h.magneticHeading;
      const cam: CameraPose = {
        yawDeg,
        pitchDeg: motionRef.current?.pitch ?? 0,
        rollDeg: motionRef.current?.roll ?? 0,
        hfovDeg: HFOV_DEG,
        width: window.innerWidth,
        height: window.innerHeight,
      };

      inFlight = true;
      // TODO(measured-in-sandbox-only): scene.rs's project_compute_cost measured the
      // Rust-side compute at ~24-162us/call, well under the plan's ~5ms fallback
      // threshold, but that excludes IPC/JSON overhead — this environment has no
      // display server to run the real WebView. Wrap this call in performance.now() on
      // a device before trusting that the full round trip is still comfortably fast.
      commands
        .projectLabels(cam)
        .then((labels) => setPlacedLabels(labels))
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
    </div>
  );
}
