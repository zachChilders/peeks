import AVFoundation
import CoreLocation
import SwiftRs
import Tauri
import UIKit
import WebKit

class StartHeadingArgs: Decodable {
  let channel: Channel
}

class CameraPlugin: Plugin, CLLocationManagerDelegate {
  private weak var webview: WKWebView?
  private let captureSession = AVCaptureSession()
  private var previewLayer: AVCaptureVideoPreviewLayer?
  private var isCameraRunning = false

  private let locationManager = CLLocationManager()
  private var headingChannel: Channel?

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
}

@_cdecl("init_plugin_camera")
func initPlugin() -> Plugin {
  return CameraPlugin()
}
