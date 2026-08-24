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
import { enu, greatCircleDistance, type Geodetic } from "./lib/geo";
import { project, layoutLabels, type CameraPose, type PlacedLabel } from "./lib/projection";
import { fetchPeaks, peakToGeodetic, type Peak } from "./lib/peaks";
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

export default function CameraView({ onClose }: { onClose: () => void }) {
  const [heading, setHeading] = useState<HeadingReading | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [placedLabels, setPlacedLabels] = useState<PlacedLabel[]>([]);

  // Sensor readings arrive far faster than we want to re-layout labels; a periodic
  // interval reads these refs instead of re-rendering on every single event.
  const headingRef = useRef<HeadingReading | null>(null);
  const motionRef = useRef<MotionReading | null>(null);
  const peaksRef = useRef<Peak[]>([]);
  const observerRef = useRef<Geodetic | null>(null);
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

  // Observer position + ground elevation, then nearby named peaks. Fetched once — no
  // re-fetch-on-movement threshold in this v0 pass (see lib/peaks.ts for the fuller
  // scope-cut rationale: no local DEM on mobile yet, so no visibility filtering either).
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
        observerRef.current = {
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
        if (!cancelled) peaksRef.current = peaks;
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

  // Re-project + re-layout on a fixed cadence, decoupled from sensor arrival rate.
  useEffect(() => {
    const id = setInterval(() => {
      const observer = observerRef.current;
      const peaks = peaksRef.current;
      const h = headingRef.current;
      if (!observer || peaks.length === 0 || !h) return;

      const yawDeg = h.trueHeading >= 0 ? h.trueHeading : h.magneticHeading;
      const cam: CameraPose = {
        yawDeg,
        pitchDeg: motionRef.current?.pitch ?? 0,
        rollDeg: motionRef.current?.roll ?? 0,
        hfovDeg: HFOV_DEG,
        width: window.innerWidth,
        height: window.innerHeight,
      };

      const margin = 80;
      const candidates: { name: string; anchor: [number, number]; dist: number }[] = [];
      for (const p of peaks) {
        const v = enu(observer, peakToGeodetic(p));
        const projected = project(cam, v);
        if (!projected) continue;
        const [x, y] = projected;
        if (x < -margin || x > cam.width + margin || y < -margin || y > cam.height + margin) {
          continue;
        }
        candidates.push({
          name: p.name,
          anchor: [x, y],
          dist: greatCircleDistance(observer, peakToGeodetic(p)),
        });
      }
      candidates.sort((a, b) => a.dist - b.dist);

      if (!measureCtxRef.current) {
        const canvas = document.createElement("canvas");
        measureCtxRef.current = canvas.getContext("2d");
      }
      const ctx = measureCtxRef.current;
      const measure = (text: string): [number, number] => {
        if (!ctx) return [text.length * 8, 18];
        ctx.font = LABEL_FONT;
        return [ctx.measureText(text).width, 18];
      };

      setPlacedLabels(layoutLabels(candidates, measure));
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

      <div className="ar-labels">
        {placedLabels.map((label) => (
          <div key={label.name}>
            <div
              className="ar-dot"
              style={{ left: label.anchor[0], top: label.anchor[1] }}
            />
            {label.rect && (
              <>
                <svg className="ar-leader">
                  <line
                    x1={label.rect.x + label.rect.w / 2}
                    y1={label.rect.y + label.rect.h}
                    x2={label.anchor[0]}
                    y2={label.anchor[1]}
                  />
                </svg>
                <div
                  className="ar-label"
                  style={{ left: label.rect.x, top: label.rect.y }}
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
