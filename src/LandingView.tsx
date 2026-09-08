import { useEffect, useSyncExternalStore } from "react";
import {
  orientationSnapshot,
  retryOrientation,
  startOrientation,
  subscribeOrientation,
  type OrientationState,
} from "./lib/orientation";
import DebugDrawer from "./DebugDrawer";
import "./LandingView.css";

/** The two things the app does, and a readout of how far along the orientation pipeline
 * is while you decide which one you want.
 *
 * The readout is not decoration: it is the reason this screen exists. Everything it
 * lists — position, visible peaks, terrain horizon, compass — has to finish before the
 * AR view can point at anything, and doing it here means the time is spent while you are
 * choosing rather than while you are holding the phone up at a ridge. */
export default function LandingView({
  onOpenMap,
  onOpenCamera,
}: {
  onOpenMap: () => void;
  onOpenCamera: () => void;
}) {
  const orientation = useSyncExternalStore(subscribeOrientation, orientationSnapshot);

  useEffect(() => {
    startOrientation();
  }, []);

  return (
    <div className="landing-view">
      <header className="landing-header">
        <h1 className="landing-title">Peeks</h1>
        <p className="landing-tagline">Know what you&rsquo;re looking at.</p>
      </header>

      <div className="landing-choices">
        <button type="button" className="landing-choice" onClick={onOpenMap}>
          <svg viewBox="0 0 24 24" width="30" height="30" aria-hidden="true">
            <path
              fill="none"
              stroke="currentColor"
              strokeWidth="1.6"
              strokeLinejoin="round"
              d="M9 4 3 6.5v13L9 17l6 2.5 6-2.5v-13L15 6.5 9 4zm0 0v13m6-10.5v13"
            />
          </svg>
          <span className="landing-choice-label">Topo map</span>
          <span className="landing-choice-sub">Where you are, and how high</span>
        </button>

        <button type="button" className="landing-choice" onClick={onOpenCamera}>
          <svg viewBox="0 0 24 24" width="30" height="30" aria-hidden="true">
            <path
              fill="currentColor"
              d="M9 3L7.17 5H4a2 2 0 0 0-2 2v11a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V7a2 2 0 0 0-2-2h-3.17L15 3H9zm3 5a5.5 5.5 0 1 1 0 11 5.5 5.5 0 0 1 0-11zm0 2a3.5 3.5 0 1 0 0 7 3.5 3.5 0 0 0 0-7z"
            />
          </svg>
          <span className="landing-choice-label">Camera ID</span>
          <span className="landing-choice-sub">Name the peaks in front of you</span>
        </button>
      </div>

      <OrientationStatus orientation={orientation} />

      <DebugDrawer />
    </div>
  );
}

/** One row per stage of the pipeline, in the order they complete. */
function OrientationStatus({ orientation }: { orientation: OrientationState }) {
  const { step, detail, error, observer, peakCount, sceneReady, headingReady, motionReady } =
    orientation;
  const done = step === "ready";

  const stages: { label: string; done: boolean; active: boolean }[] = [
    { label: "Position", done: observer !== null, active: step === "locating" },
    {
      label: peakCount !== null ? `Visible peaks (${peakCount})` : "Visible peaks",
      done: sceneReady,
      active: step === "peaks",
    },
    { label: "Terrain horizon", done, active: step === "horizon" },
    // Compass and gyro settle on the sensors' own schedule rather than in sequence with
    // the rest, which is exactly why they are started here and not when the camera opens.
    { label: "Compass", done: headingReady && motionReady, active: !headingReady },
  ];

  return (
    <div className="landing-status">
      <div className="landing-status-headline">
        {step === "error" ? (
          <>
            <span className="landing-status-error">Orienting failed: {error}</span>
            <button type="button" className="landing-retry" onClick={retryOrientation}>
              Retry
            </button>
          </>
        ) : (
          <span>{done ? "Oriented — ready to point" : `Orienting: ${detail}`}</span>
        )}
      </div>
      {/* A sensor that reported a problem without stopping the pipeline — the compass
          going unavailable, say. Worth showing, but not a failed orientation. */}
      {step !== "error" && error && <div className="landing-status-error">{error}</div>}
      <ul className="landing-stages">
        {stages.map((stage) => (
          <li
            key={stage.label}
            className={`landing-stage${stage.done ? " landing-stage-done" : ""}${
              stage.active && !stage.done ? " landing-stage-active" : ""
            }`}
          >
            <span className="landing-stage-dot" aria-hidden="true" />
            {stage.label}
          </li>
        ))}
      </ul>
    </div>
  );
}
