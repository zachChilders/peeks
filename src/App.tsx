import { useRef, useState } from "react";
import LandingView from "./LandingView";
import MapView from "./MapView";
import CameraView from "./CameraView";

type View = "landing" | "map" | "camera";

function App() {
  const [view, setView] = useState<View>("landing");
  // Where the camera was opened from, so closing it goes back there rather than always
  // dropping to the landing page — the map's camera shortcut would otherwise lose your
  // place on the map every time.
  const cameraOriginRef = useRef<View>("landing");

  function openCamera(from: View) {
    cameraOriginRef.current = from;
    setView("camera");
  }

  switch (view) {
    case "camera":
      return <CameraView onClose={() => setView(cameraOriginRef.current)} />;
    case "map":
      return (
        <MapView onBack={() => setView("landing")} onOpenCamera={() => openCamera("map")} />
      );
    case "landing":
      return (
        <LandingView
          onOpenMap={() => setView("map")}
          onOpenCamera={() => openCamera("landing")}
        />
      );
  }
}

export default App;
