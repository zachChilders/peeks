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
