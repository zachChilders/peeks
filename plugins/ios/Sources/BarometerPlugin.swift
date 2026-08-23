import CoreMotion
import SwiftRs
import Tauri
import UIKit
import WebKit

class StartUpdatesArgs: Decodable {
  let channel: Channel
}

class BarometerPlugin: Plugin {
  private let altimeter = CMAltimeter()
  private var updateChannel: Channel? = nil

  @objc public func isAvailable(_ invoke: Invoke) throws {
    invoke.resolve(["available": CMAltimeter.isRelativeAltitudeAvailable()])
  }

  @objc public func startUpdates(_ invoke: Invoke) throws {
    guard CMAltimeter.isRelativeAltitudeAvailable() else {
      invoke.reject("Relative altitude is not available on this device.")
      return
    }

    let args = try invoke.parseArgs(StartUpdatesArgs.self)
    self.updateChannel = args.channel

    altimeter.startRelativeAltitudeUpdates(to: OperationQueue.main) { data, error in
      if let error = error {
        do {
          try self.updateChannel?.send(error.localizedDescription)
        } catch {
          Logger.error(error)
        }
        return
      }

      guard let data = data else { return }

      let reading: JsonObject = [
        "relativeAltitude": data.relativeAltitude.doubleValue,
        "pressure": data.pressure.doubleValue,
        "timestamp": Int(Date().timeIntervalSince1970 * 1000),
      ]

      do {
        try self.updateChannel?.send(reading)
      } catch {
        Logger.error(error)
      }
    }

    invoke.resolve()
  }

  @objc public func stopUpdates(_ invoke: Invoke) throws {
    altimeter.stopRelativeAltitudeUpdates()
    self.updateChannel = nil
    invoke.resolve()
  }
}

@_cdecl("init_plugin_barometer")
func initPlugin() -> Plugin {
  return BarometerPlugin()
}
