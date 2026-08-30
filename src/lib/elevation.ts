// Ground elevation, via the get_elevation Tauri command (local Copernicus GLO-30 DEM,
// the same one the AR view's terrain-occlusion check samples).
//
// Both MapView and CameraView need a single-point elevation lookup; this used to be a
// verbatim-duplicated fetch() in each file. Both now call the same Rust command.

import { commands } from "../bindings";

export async function fetchElevation(lat: number, lon: number): Promise<number> {
  const result = await commands.getElevation(lat, lon);
  if (result.status === "error") throw new Error(result.error);
  // Generated bindings type every f64 field `number | null` (serde_json encodes
  // NaN/Infinity as null), which this result never is in practice.
  return result.data!;
}
