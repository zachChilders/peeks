import { useEffect, useState } from "react";
import {
  startCamera,
  stopCamera,
  startHeadingUpdates,
  stopHeadingUpdates,
  type HeadingReading,
} from "tauri-plugin-camera-api";
import "./CameraView.css";

const CARDINALS = [
  "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE",
  "S", "SSW", "SW", "WSW", "W", "WNW", "NW", "NNW",
];

function cardinal(deg: number): string {
  return CARDINALS[Math.round(deg / 22.5) % 16];
}

export default function CameraView({ onClose }: { onClose: () => void }) {
  const [heading, setHeading] = useState<HeadingReading | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;

    async function start() {
      try {
        await startCamera();
      } catch (e) {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      }

      try {
        await startHeadingUpdates((reading, err) => {
          if (err) {
            setError(err);
            return;
          }
          if (reading) setHeading(reading);
        });
      } catch (e) {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      }
    }

    start();

    return () => {
      cancelled = true;
      stopHeadingUpdates().catch(() => {});
      stopCamera().catch(() => {});
    };
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
