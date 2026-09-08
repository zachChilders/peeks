import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import {
  startCamera,
  stopCamera,
  startIntrinsicsUpdates,
  stopIntrinsicsUpdates,
  capturePhoto,
  type HeadingReading,
  type CameraIntrinsicsReading,
} from "tauri-plugin-camera-api";
import {
  commands,
  type CalibrationStatus,
  type CameraIntrinsics,
  type CameraPose,
  type PlacedLabel,
} from "./bindings";
import {
  currentFix,
  currentHeading,
  currentMotion,
  isSceneReady,
  orientationSnapshot,
  refreshOrientation,
  subscribeHeading,
  subscribeOrientation,
} from "./lib/orientation";
import { log } from "./lib/debugLog";
import DebugDrawer from "./DebugDrawer";
import "./CameraView.css";

const CARDINALS = [
  "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE",
  "S", "SSW", "SW", "WSW", "W", "WNW", "NW", "NNW",
];

function cardinal(deg: number): string {
  return CARDINALS[Math.round(deg / 22.5) % 16];
}

// Fallback on-screen horizontal FOV, used only for the few ticks before the first
// intrinsics reading arrives from the camera plugin. It is a poor stand-in — the real
// value on a portrait phone is closer to 35 deg once the resizeAspectFill crop is
// accounted for (see CameraIntrinsics in peakcore's projection.rs) — so anything that
// depends on accurate placement should wait for real intrinsics rather than trust this.
// The fitter enforces exactly that: `ingest_frame` refuses to fit without real
// intrinsics, so no ticks projected with this value can reach the calibration datum.
const FALLBACK_HFOV_DEG = 63;
const PROJECTION_INTERVAL_MS = 100;

/** Drops the plugin reading's `timestamp` to get the shape the projection expects. */
function toCameraIntrinsics(reading: CameraIntrinsicsReading): CameraIntrinsics {
  return {
    fovDeg: reading.fovDeg,
    zoomFactor: reading.zoomFactor,
    bufferLongPx: reading.bufferLongPx,
    bufferShortPx: reading.bufferShortPx,
  };
}

/** The AR overlay. Everything slow was done before this mounted — see `lib/orientation`,
 * which holds the position, the visible-peak scene, the terrain horizon and the settled
 * compass/gyro streams across views. What is left here is the part that genuinely needs
 * the capture device: the preview, its intrinsics, the skyline fitter's frame stream, and
 * the projection tick that draws labels. */
export default function CameraView({ onClose }: { onClose: () => void }) {
  const [heading, setHeading] = useState<HeadingReading | null>(currentHeading);
  const [error, setError] = useState<string | null>(null);
  const [placedLabels, setPlacedLabels] = useState<PlacedLabel[]>([]);
  const [horizonSegments, setHorizonSegments] = useState<[number, number][][]>([]);
  const [capturing, setCapturing] = useState(false);
  const [captureFlash, setCaptureFlash] = useState(false);
  const [calibration, setCalibration] = useState<CalibrationStatus | null>(null);
  // Orientation is prepared a view earlier, so its failures (no position fix, no compass)
  // have to surface here too — otherwise the overlay just silently never appears.
  const orientation = useSyncExternalStore(subscribeOrientation, orientationSnapshot);

  // Intrinsics arrive far faster than we want to re-layout labels, and only the
  // projection tick reads them; a ref keeps them out of the render path entirely. Heading
  // and motion live in lib/orientation for the same reason, one level up.
  const intrinsicsRef = useRef<CameraIntrinsics | null>(null);

  async function onCapture() {
    if (capturing) return;
    setCapturing(true);
    try {
      // Started before the shutter and awaited after it, for two reasons: the position
      // recorded is then contemporaneous with the capture rather than however long
      // writing the asset took, and a slow fix cannot hold up the shutter flash below.
      // `currentFix` resolves to null rather than throwing, so nothing is left unhandled
      // if the capture itself fails first.
      const fixInFlight = currentFix();

      const photo = await capturePhoto();
      log(`capture: saved to Photos as ${photo.fileName}`);
      // Brief shutter flash — the only feedback a capture happened, since there's no
      // shutter sound/animation from the native side.
      setCaptureFlash(true);
      setTimeout(() => setCaptureFlash(false), 150);

      const fix = await fixInFlight;

      if (!fix) {
        // The photo is saved either way; only the coordinates are lost, and the log is
        // where that has to be visible, since nothing on screen would show it.
        log(`ERROR [logPhoto]: no position for ${photo.fileName}, not recorded`);
        setError("[logPhoto] no position available; photo saved without coordinates");
        return;
      }

      const logged = await commands.logPhoto({
        fileName: photo.fileName,
        localIdentifier: photo.localIdentifier,
        lat: fix.lat,
        lon: fix.lon,
        altitudeM: fix.altitudeM,
        accuracyM: fix.accuracyM,
      });
      if (logged.status === "error") {
        log(`ERROR [logPhoto]: ${logged.error}`);
        setError(`[logPhoto] ${logged.error}`);
        return;
      }
      log(`capture: recorded at ${fix.lat.toFixed(5)}, ${fix.lon.toFixed(5)}`);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      log(`ERROR [capturePhoto]: ${msg}`);
      setError(`[capturePhoto] ${msg}`);
    } finally {
      setCapturing(false);
    }
  }

  // One render per compass reading, which is what the heading readout below wants. The
  // projection tick reads the same value through `currentHeading()` instead, so it is not
  // coupled to this.
  useEffect(() => subscribeHeading(setHeading), []);

  // Camera preview + intrinsics + skyline fitter lifecycle. The compass and gyro are
  // deliberately *not* here: they were started on the landing page and stay running, so
  // arriving at this view doesn't pay the magnetometer's settling time again.
  useEffect(() => {
    let cancelled = false;

    async function start() {
      // The scene was built for wherever the phone was when the landing page loaded.
      // Rebuild it if that has stopped being true; the existing one stays live meanwhile.
      void refreshOrientation();

      try {
        await startCamera();
      } catch (e) {
        if (!cancelled) setError(`[startCamera] ${e instanceof Error ? e.message : String(e)}`);
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
      stopCamera().catch(() => {});
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
    // The frame geometry the fitter is actually running against, logged once when it
    // first arrives. Constant for a session, and the one thing a "poor match" line cannot
    // tell you on its own: see CalibrationStatus::frame_w in calibration.rs.
    let loggedFrame = "";

    const id = setInterval(() => {
      if (inFlight || !isSceneReady()) return;
      const h = currentHeading();
      if (!h) return;
      const motion = currentMotion();

      const yawDeg = h.trueHeading >= 0 ? h.trueHeading : h.magneticHeading;
      const cam: CameraPose = {
        // The compass reading. Whether the overlay is actually pointed by it or by the
        // gyro datum below is decided in Rust (src-tauri/src/calibration.rs); once the
        // skyline fitter has locked, this stops being consulted.
        yawDeg,
        pitchDeg: motion?.pitch ?? 0,
        rollDeg: motion?.roll ?? 0,
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
        .projectLabels(cam, motion?.relativeYawDeg ?? null)
        .then(({ labels, horizon, effectiveHfovDeg, calibration }) => {
          setPlacedLabels(labels);
          // Already split into strokeable runs and culled to the viewport by
          // `peakcore::projection::project_horizon` — see that function for why deciding
          // where the line breaks needs the camera geometry rather than a screen-distance
          // heuristic. Nothing left to do here but read the numbers.
          setHorizonSegments(horizon.map((seg) => seg.map(([x, y]) => [x!, y!])));

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
          if (calibration.frameW > 0) {
            const frame = `${calibration.frameW}x${calibration.frameH} f=${calibration.frameFocalPx!.toFixed(0)}px`;
            if (frame !== loggedFrame) {
              loggedFrame = frame;
              log(`fit: frame ${frame}`);
            }
          }
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

  const banner = error ?? orientation.error;

  return (
    <div className="camera-view">
      <div className="camera-overlay">
        {banner && <div className="camera-overlay-error">{banner}</div>}
        {degrees !== null ? (
          <div className="camera-heading">
            {degrees.toFixed(0)}&deg; {cardinal(degrees)}
          </div>
        ) : (
          !banner && <div className="camera-heading">Orienting&hellip;</div>
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

      {/* Once locked, the heading is held on the gyro datum and the compass is out of the
          loop, so this line is the only place the compass error is visible at all — and
          the age is how long the datum has been coasting on gyro drift since anything last
          corrected it. Without both, an overlay silently sliding is indistinguishable from
          one that is simply right. */}
      {calibration?.locked && (
        <div className="camera-calibration">
          compass {calibration.dYawDeg! >= 0 ? "+" : ""}
          {calibration.dYawDeg!.toFixed(1)}&deg; / pitch {calibration.dPitchDeg! >= 0 ? "+" : ""}
          {calibration.dPitchDeg!.toFixed(1)}&deg;
          <span className="camera-calibration-rate">
            {" "}
            {calibration.accepted}/{calibration.frames}
            {calibration.lockAgeS !== null && ` · ${calibration.lockAgeS!.toFixed(0)}s ago`}
          </span>
        </div>
      )}

      <DebugDrawer />

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
