import { useState } from "react";
import MapView from "./MapView";
import CameraView from "./CameraView";

function App() {
  const [showCamera, setShowCamera] = useState(false);

  return showCamera ? (
    <CameraView onClose={() => setShowCamera(false)} />
  ) : (
    <MapView onOpenCamera={() => setShowCamera(true)} />
  );
}

export default App;
