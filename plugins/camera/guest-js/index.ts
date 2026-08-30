import { Channel, invoke } from '@tauri-apps/api/core'

export type HeadingReading = {
  /**
   * Heading relative to magnetic north, in degrees (0-360).
   */
  magneticHeading: number
  /**
   * Heading relative to true north, in degrees (0-360). May equal magnetic heading
   * if true-north correction is unavailable.
   */
  trueHeading: number
  /**
   * Accuracy of the heading in degrees; negative means invalid.
   */
  accuracy: number
  timestamp: number
}

/**
 * Start the native camera preview. The webview's background is made transparent so the
 * preview (layered behind it) shows through — render your AR UI in HTML on top of it.
 * iOS only.
 */
export async function startCamera(): Promise<void> {
  await invoke('plugin:camera|start_camera')
}

export async function stopCamera(): Promise<void> {
  await invoke('plugin:camera|stop_camera')
}

/**
 * Start streaming compass heading updates.
 */
export async function startHeadingUpdates(
  cb: (reading: HeadingReading | null, error?: string) => void
): Promise<number> {
  const channel = new Channel<HeadingReading | string>()
  channel.onmessage = (message) => {
    if (typeof message === 'string') {
      cb(null, message)
    } else {
      cb(message)
    }
  }
  await invoke('plugin:camera|start_heading_updates', { channel })
  return channel.id
}

export async function stopHeadingUpdates(): Promise<void> {
  await invoke('plugin:camera|stop_heading_updates')
}

export type MotionReading = {
  /**
   * Camera tilt above horizontal, in degrees (0 = level, +90 = pointing at zenith).
   */
  pitch: number
  /**
   * Rotation about the camera's optical axis, in degrees (0 = top of phone points up).
   */
  roll: number
  timestamp: number
}

/**
 * Start streaming device-motion (pitch/roll) updates, derived from the gravity vector
 * for a phone held upright as a camera viewfinder.
 */
export async function startMotionUpdates(
  cb: (reading: MotionReading | null, error?: string) => void
): Promise<number> {
  const channel = new Channel<MotionReading | string>()
  channel.onmessage = (message) => {
    if (typeof message === 'string') {
      cb(null, message)
    } else {
      cb(message)
    }
  }
  await invoke('plugin:camera|start_motion_updates', { channel })
  return channel.id
}

export async function stopMotionUpdates(): Promise<void> {
  await invoke('plugin:camera|stop_motion_updates')
}

export type CameraIntrinsicsReading = {
  /**
   * Field of view across the capture buffer's long axis, in degrees, at zoom 1.0.
   * Held portrait, that axis maps to screen height — not width.
   */
  fovDeg: number
  /**
   * Current zoom, relative to the widest lens. 1.0 = unzoomed.
   */
  zoomFactor: number
  /**
   * Capture buffer dimensions in the sensor's native landscape orientation.
   */
  bufferLongPx: number
  bufferShortPx: number
  timestamp: number
}

/**
 * Start streaming capture-device intrinsics: the native field of view, the current zoom
 * factor, and the capture buffer size. Feed these to the AR projection so it derives a
 * real focal length rather than assuming an on-screen FOV — the assumption is off by
 * roughly 2x once the `resizeAspectFill` crop is accounted for.
 *
 * Emits once immediately, then on every zoom change. Requires the camera to already be
 * running (`startCamera`), since the intrinsics come from the active capture device.
 */
export async function startIntrinsicsUpdates(
  cb: (reading: CameraIntrinsicsReading | null, error?: string) => void
): Promise<number> {
  const channel = new Channel<CameraIntrinsicsReading | string>()
  channel.onmessage = (message) => {
    if (typeof message === 'string') {
      cb(null, message)
    } else {
      cb(message)
    }
  }
  await invoke('plugin:camera|start_intrinsics_updates', { channel })
  return channel.id
}

export async function stopIntrinsicsUpdates(): Promise<void> {
  await invoke('plugin:camera|stop_intrinsics_updates')
}

/**
 * Snapshot the camera preview plus the AR overlay on top of it (labels, leader lines,
 * anything else rendered in HTML), and save the result to the Photos library. Prompts
 * for add-only Photos access on first use. iOS only.
 */
export async function capturePhoto(): Promise<void> {
  await invoke('plugin:camera|capture_photo')
}
