import { Channel, invoke } from '@tauri-apps/api/core'

export type AltitudeReading = {
  /**
   * Change in altitude in meters since updates started (positive = up).
   */
  relativeAltitude: number
  /**
   * Atmospheric pressure in kilopascals.
   */
  pressure: number
  timestamp: number
}

/**
 * Whether this device has a barometer capable of relative altitude updates.
 * iOS only — always resolves false on desktop.
 */
export async function isAvailable(): Promise<boolean> {
  return await invoke<boolean>('plugin:barometer|is_available')
}

/**
 * Start streaming barometer-derived relative altitude updates.
 * Returns a channel id to pass to `stopUpdates`.
 */
export async function startUpdates(
  cb: (reading: AltitudeReading | null, error?: string) => void
): Promise<number> {
  const channel = new Channel<AltitudeReading | string>()
  channel.onmessage = (message) => {
    if (typeof message === 'string') {
      cb(null, message)
    } else {
      cb(message)
    }
  }
  await invoke('plugin:barometer|start_updates', { channel })
  return channel.id
}

export async function stopUpdates(): Promise<void> {
  await invoke('plugin:barometer|stop_updates')
}
