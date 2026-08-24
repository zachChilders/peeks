import { useEffect, useRef, useState } from "react";
import L from "leaflet";
import "leaflet/dist/leaflet.css";
import markerIcon2x from "leaflet/dist/images/marker-icon-2x.png";
import markerIcon from "leaflet/dist/images/marker-icon.png";
import markerShadow from "leaflet/dist/images/marker-shadow.png";
import {
  requestPermissions,
  getCurrentPosition,
  watchPosition,
  clearWatch,
  type Position,
} from "@tauri-apps/plugin-geolocation";
import {
  isAvailable as isBarometerAvailable,
  startUpdates as startBarometerUpdates,
  stopUpdates as stopBarometerUpdates,
  type AltitudeReading,
} from "tauri-plugin-barometer-api";
import { fetchElevation } from "./lib/elevation";
import "./MapView.css";

L.Icon.Default.mergeOptions({
  iconRetinaUrl: markerIcon2x,
  iconUrl: markerIcon,
  shadowUrl: markerShadow,
});

const ELEVATION_MIN_INTERVAL_MS = 15_000;

function metersToFeet(m: number): number {
  return m * 3.28084;
}

export default function MapView({ onOpenCamera }: { onOpenCamera: () => void }) {
  const mapContainerRef = useRef<HTMLDivElement | null>(null);
  const mapRef = useRef<L.Map | null>(null);
  const markerRef = useRef<L.Marker | null>(null);
  const lastElevationFetchRef = useRef(0);

  const [position, setPosition] = useState<Position | null>(null);
  const [elevation, setElevation] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Barometer: a device sensor reading (relative altitude change since app start), calibrated
  // against the DEM elevation lookup once both are available. Falls back to the DEM/GPS values
  // above when unavailable (e.g. simulator, older devices).
  const [baroReading, setBaroReading] = useState<AltitudeReading | null>(null);
  const [baroBaseline, setBaroBaseline] = useState<{
    demElevation: number;
    relativeAltitude: number;
  } | null>(null);

  // Initialize the map once.
  useEffect(() => {
    if (!mapContainerRef.current || mapRef.current) return;

    const map = L.map(mapContainerRef.current).setView([46.8523, -121.7603], 12);
    L.tileLayer("https://{s}.tile.opentopomap.org/{z}/{x}/{y}.png", {
      maxZoom: 17,
      attribution:
        'Map data: &copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a> contributors, SRTM | ' +
        'Map style: &copy; <a href="https://opentopomap.org">OpenTopoMap</a> (CC-BY-SA)',
    }).addTo(map);
    mapRef.current = map;

    return () => {
      map.remove();
      mapRef.current = null;
    };
  }, []);

  // Track device position.
  useEffect(() => {
    let watchId: number | null = null;
    let cancelled = false;

    async function start() {
      try {
        const permission = await requestPermissions(["location"]);
        if (permission.location !== "granted" && permission.location !== "prompt-with-rationale") {
          setError("Location permission denied.");
          return;
        }

        const initial = await getCurrentPosition();
        if (!cancelled) setPosition(initial);

        watchId = await watchPosition(
          { enableHighAccuracy: true, timeout: 10_000, maximumAge: 0 },
          (pos, err) => {
            if (err) {
              setError(err);
              return;
            }
            if (pos) setPosition(pos);
          },
        );
      } catch (e) {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      }
    }

    start();

    return () => {
      cancelled = true;
      if (watchId !== null) clearWatch(watchId);
    };
  }, []);

  // Stream barometer updates, if the device has one.
  useEffect(() => {
    let cancelled = false;

    async function start() {
      try {
        const available = await isBarometerAvailable();
        if (cancelled || !available) return;

        await startBarometerUpdates((reading, err) => {
          if (err) {
            setError(err);
            return;
          }
          if (reading) setBaroReading(reading);
        });
      } catch (e) {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      }
    }

    start();

    return () => {
      cancelled = true;
      stopBarometerUpdates().catch(() => {});
    };
  }, []);

  // Calibrate the barometer against the DEM elevation lookup the first time both are available.
  useEffect(() => {
    if (!baroReading || elevation === null || baroBaseline) return;
    setBaroBaseline({ demElevation: elevation, relativeAltitude: baroReading.relativeAltitude });
  }, [baroReading, elevation, baroBaseline]);

  function recenter() {
    if (!position || !mapRef.current) return;
    mapRef.current.setView(
      [position.coords.latitude, position.coords.longitude],
      Math.max(mapRef.current.getZoom(), 14),
    );
  }

  // Move the marker/map and refresh elevation when position changes.
  useEffect(() => {
    if (!position || !mapRef.current) return;
    const { latitude, longitude } = position.coords;

    if (!markerRef.current) {
      markerRef.current = L.marker([latitude, longitude]).addTo(mapRef.current);
      mapRef.current.setView([latitude, longitude], 14);
    } else {
      markerRef.current.setLatLng([latitude, longitude]);
    }

    const now = Date.now();
    if (now - lastElevationFetchRef.current > ELEVATION_MIN_INTERVAL_MS) {
      lastElevationFetchRef.current = now;
      fetchElevation(latitude, longitude)
        .then(setElevation)
        .catch((e) => setError(e instanceof Error ? e.message : String(e)));
    }
  }, [position]);

  const baroElevation =
    baroBaseline && baroReading
      ? baroBaseline.demElevation + (baroReading.relativeAltitude - baroBaseline.relativeAltitude)
      : null;
  const displayElevation = baroElevation ?? elevation;
  const elevationSource = baroElevation !== null ? "barometer" : elevation !== null ? "DEM estimate" : null;

  return (
    <div className="map-view">
      <div ref={mapContainerRef} className="map-container" />
      <div className="map-overlay">
        {error && <div className="map-overlay-error">{error}</div>}
        {position && (
          <>
            <div>
              {position.coords.latitude.toFixed(5)}, {position.coords.longitude.toFixed(5)}
            </div>
            <div>
              Elevation:{" "}
              {displayElevation !== null
                ? `${displayElevation.toFixed(0)} m / ${metersToFeet(displayElevation).toFixed(0)} ft`
                : "loading…"}
              {elevationSource && (
                <span className="map-overlay-secondary"> ({elevationSource})</span>
              )}
            </div>
            {position.coords.altitude !== null && (
              <div className="map-overlay-secondary">
                GPS altitude: {position.coords.altitude.toFixed(0)} m
              </div>
            )}
          </>
        )}
        {!position && !error && <div>Locating…</div>}
      </div>
      <button
        type="button"
        className="locate-button"
        onClick={recenter}
        disabled={!position}
        aria-label="Center on my location"
      >
        <svg viewBox="0 0 24 24" width="24" height="24">
          <circle cx="12" cy="12" r="3" fill="currentColor" />
          <path
            stroke="currentColor"
            strokeWidth="2"
            d="M12 2v4M12 18v4M2 12h4M18 12h4"
          />
          <circle cx="12" cy="12" r="7" stroke="currentColor" strokeWidth="2" fill="none" />
        </svg>
      </button>
      <button
        type="button"
        className="camera-button"
        onClick={onOpenCamera}
        aria-label="Open AR camera view"
      >
        <svg viewBox="0 0 24 24" width="22" height="22">
          <path
            fill="currentColor"
            d="M9 3L7.17 5H4a2 2 0 0 0-2 2v11a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V7a2 2 0 0 0-2-2h-3.17L15 3H9zm3 5a5.5 5.5 0 1 1 0 11 5.5 5.5 0 0 1 0-11zm0 2a3.5 3.5 0 1 0 0 7 3.5 3.5 0 0 0 0-7z"
          />
        </svg>
      </button>
    </div>
  );
}
