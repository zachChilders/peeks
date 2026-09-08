// The on-device pipeline trace, shared by every view.
//
// TestFlight builds have no attached debugger, so this log is how "why are there no
// labels" gets diagnosed from a screenshot alone. It lives outside React because the
// pipeline it traces now starts on the landing page and is still running when CameraView
// mounts: a per-component useState buffer would drop every line written before the view
// that displays it existed, which is exactly the part of the pipeline that moved.

// Enough to hold a whole cold start (position, peaks, horizon, then the camera's own
// lines) now that the drawer scrolls rather than showing a fixed dozen.
const MAX_LINES = 200;

let lines: readonly string[] = [];
const listeners = new Set<() => void>();

export function log(msg: string): void {
  console.log(msg);
  lines = [...lines.slice(-(MAX_LINES - 1)), msg];
  for (const listener of listeners) listener();
}

/** Stable reference between writes, as `useSyncExternalStore` requires. */
export function debugLogSnapshot(): readonly string[] {
  return lines;
}

export function subscribeDebugLog(onChange: () => void): () => void {
  listeners.add(onChange);
  return () => {
    listeners.delete(onChange);
  };
}
