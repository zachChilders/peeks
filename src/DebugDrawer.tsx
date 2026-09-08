import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import { debugLogSnapshot, subscribeDebugLog } from "./lib/debugLog";
import "./DebugDrawer.css";

/** The pipeline trace, out of the way until asked for.
 *
 * It used to be twelve lines pinned over the camera preview at all times — which is a
 * lot of the viewfinder to spend on something only wanted when the overlay looks wrong.
 * Collapsed by default, it can hold the whole trace instead of a rolling dozen, and the
 * same drawer shows on the landing page, where most of those lines are now written. */
export default function DebugDrawer() {
  const [open, setOpen] = useState(false);
  const lines = useSyncExternalStore(subscribeDebugLog, debugLogSnapshot);
  const bodyRef = useRef<HTMLPreElement | null>(null);

  // Pin to the newest line: the tail is the part being read, and while the drawer is open
  // lines keep arriving under it.
  useEffect(() => {
    if (!open || !bodyRef.current) return;
    bodyRef.current.scrollTop = bodyRef.current.scrollHeight;
  }, [open, lines]);

  return (
    <div className={`debug-drawer${open ? " debug-drawer-open" : ""}`}>
      {open && (
        <pre className="debug-drawer-body" ref={bodyRef}>
          {lines.join("\n")}
        </pre>
      )}
      <button
        type="button"
        className="debug-drawer-toggle"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
      >
        debug {open ? "▾" : "▴"}
        <span className="debug-drawer-count">{lines.length}</span>
      </button>
    </div>
  );
}
