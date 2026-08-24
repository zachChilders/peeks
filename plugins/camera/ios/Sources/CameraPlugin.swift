import AVFoundation
import CoreLocation
import CoreMotion
import SwiftRs
import Tauri
import UIKit
import WebKit

class StartHeadingArgs: Decodable {
  let channel: Channel
}

class StartMotionArgs: Decodable {
  let channel: Channel
}

class CameraPlugin: Plugin, CLLocationManagerDelegate {
  private weak var webview: WKWebView?
  private let captureSession = AVCaptureSession()
  private var previewLayer: AVCaptureVideoPreviewLayer?
  private var isCameraRunning = false

  private let locationManager = CLLocationManager()
  private var headingChannel: Channel?

  private let motionManager = CMMotionManager()
  private var motionChannel: Channel?

  override init() {
    super.init()
    locationManager.delegate = self
  }

  @objc open override func load(webview: WKWebView) {
    self.webview = webview
    NotificationCenter.default.addObserver(
      self,
      selector: #selector(updatePreviewFrame),
      name: UIDevice.orientationDidChangeNotification,
      object: nil
    )
  }

  //
  // Camera
  //

  @objc public func startCamera(_ invoke: Invoke) throws {
    AVCaptureDevice.requestAccess(for: .video) { granted in
      DispatchQueue.main.async {
        guard granted else {
          invoke.reject("Camera access denied.")
          return
        }
        guard let webview = self.webview, let container = webview.superview else {
          invoke.reject("Camera view is not attached to a window yet.")
          return
        }
        guard
          let device = AVCaptureDevice.default(
            .builtInWideAngleCamera, for: .video, position: .back),
          let input = try? AVCaptureDeviceInput(device: device)
        else {
          invoke.reject("No back camera available on this device.")
          return
        }

        if self.captureSession.inputs.isEmpty {
          self.captureSession.beginConfiguration()
          self.captureSession.sessionPreset = .high
          if self.captureSession.canAddInput(input) {
            self.captureSession.addInput(input)
          }
          self.captureSession.commitConfiguration()
        }

        // Make the webview transparent so the native camera preview shows through from behind.
        webview.isOpaque = false
        webview.backgroundColor = .clear
        webview.scrollView.backgroundColor = .clear
        webview.scrollView.isOpaque = false

        let layer = AVCaptureVideoPreviewLayer(session: self.captureSession)
        layer.videoGravity = .resizeAspectFill
        layer.frame = container.bounds
        container.layer.insertSublayer(layer, below: webview.layer)
        self.previewLayer = layer

        DispatchQueue.global(qos: .userInitiated).async {
          self.captureSession.startRunning()
        }

        self.isCameraRunning = true
        invoke.resolve()
      }
    }
  }

  @objc public func stopCamera(_ invoke: Invoke) throws {
    stopCameraInternal()
    invoke.resolve()
  }

  private func stopCameraInternal() {
    guard isCameraRunning else { return }

    DispatchQueue.global(qos: .userInitiated).async {
      self.captureSession.stopRunning()
    }

    previewLayer?.removeFromSuperlayer()
    previewLayer = nil

    webview?.isOpaque = true
    webview?.backgroundColor = nil
    webview?.scrollView.backgroundColor = nil

    isCameraRunning = false
  }

  @objc private func updatePreviewFrame() {
    guard let container = webview?.superview else { return }
    DispatchQueue.main.async {
      self.previewLayer?.frame = container.bounds
    }
  }

  //
  // Compass
  //

  @objc public func startHeadingUpdates(_ invoke: Invoke) throws {
    guard CLLocationManager.headingAvailable() else {
      invoke.reject("Compass is not available on this device.")
      return
    }

    let args = try invoke.parseArgs(StartHeadingArgs.self)
    self.headingChannel = args.channel

    locationManager.startUpdatingHeading()
    invoke.resolve()
  }

  @objc public func stopHeadingUpdates(_ invoke: Invoke) throws {
    locationManager.stopUpdatingHeading()
    self.headingChannel = nil
    invoke.resolve()
  }

  func locationManager(_ manager: CLLocationManager, didUpdateHeading newHeading: CLHeading) {
    let reading: JsonObject = [
      "magneticHeading": newHeading.magneticHeading,
      "trueHeading": newHeading.trueHeading,
      "accuracy": newHeading.headingAccuracy,
      "timestamp": Int(newHeading.timestamp.timeIntervalSince1970 * 1000),
    ]

    headingChannel?.send(reading)
  }

  //
  // Device motion (pitch/roll, for a phone held upright as an AR viewfinder)
  //

  @objc public func startMotionUpdates(_ invoke: Invoke) throws {
    guard motionManager.isDeviceMotionAvailable else {
      invoke.reject("Device motion is not available on this device.")
      return
    }

    let args = try invoke.parseArgs(StartMotionArgs.self)
    self.motionChannel = args.channel

    motionManager.deviceMotionUpdateInterval = 1.0 / 30.0
    motionManager.startDeviceMotionUpdates(to: .main) { motion, error in
      if let error = error {
        do {
          try self.motionChannel?.send(error.localizedDescription)
        } catch {
          Logger.error(error)
        }
        return
      }
      guard let motion = motion else { return }

      // Derived directly from the gravity vector rather than `motion.attitude`, because
      // CMAttitude's pitch/roll Euler angles are defined for a device held flat
      // (rotation about the fixed local X/Y axes), not for a phone held upright as a
      // camera viewfinder. Here the camera's optical axis is the device's local -Z, so:
      //   pitch = angle of the camera axis above horizontal (0 = level, +90 = zenith)
      //   roll  = rotation about the camera axis (0 = top of phone points up)
      // Unverified on real hardware — Simulator has no motion sensors to check against.
      // If pitch or roll reads inverted on a real device, flip the corresponding sign.
      let g = motion.gravity
      let pitch = atan2(g.z, -g.y) * 180.0 / .pi
      let roll = atan2(g.x, -g.y) * 180.0 / .pi

      let reading: JsonObject = [
        "pitch": pitch,
        "roll": roll,
        "timestamp": Int(Date().timeIntervalSince1970 * 1000),
      ]
      self.motionChannel?.send(reading)
    }

    invoke.resolve()
  }

  @objc public func stopMotionUpdates(_ invoke: Invoke) throws {
    motionManager.stopDeviceMotionUpdates()
    self.motionChannel = nil
    invoke.resolve()
  }
}

@_cdecl("init_plugin_camera")
func initPlugin() -> Plugin {
  return CameraPlugin()
}
