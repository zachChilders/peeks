import AVFoundation
import CoreLocation
import CoreMotion
import Photos
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

class StartIntrinsicsArgs: Decodable {
  let channel: Channel
}

class CameraPlugin: Plugin, CLLocationManagerDelegate, AVCapturePhotoCaptureDelegate {
  private weak var webview: WKWebView?
  private let captureSession = AVCaptureSession()
  private let photoOutput = AVCapturePhotoOutput()
  private var previewLayer: AVCaptureVideoPreviewLayer?
  private var isCameraRunning = false
  private var currentDevice: AVCaptureDevice?
  private weak var pinchGesture: UIPinchGestureRecognizer?
  private var pinchStartZoomFactor: CGFloat?
  private var pendingCaptureInvoke: Invoke?

  private let locationManager = CLLocationManager()
  private var headingChannel: Channel?

  private let motionManager = CMMotionManager()
  private var motionChannel: Channel?

  private var intrinsicsChannel: Channel?
  private var zoomObservation: NSKeyValueObservation?

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
          let device = self.backCameraDevice(),
          let input = try? AVCaptureDeviceInput(device: device)
        else {
          invoke.reject("No back camera available on this device.")
          return
        }
        self.currentDevice = device

        if self.captureSession.inputs.isEmpty {
          self.captureSession.beginConfiguration()
          self.captureSession.sessionPreset = .high
          if self.captureSession.canAddInput(input) {
            self.captureSession.addInput(input)
          }
          if self.captureSession.canAddOutput(self.photoOutput) {
            self.captureSession.addOutput(self.photoOutput)
          }
          self.captureSession.commitConfiguration()
        }

        // Make the webview transparent so the native camera preview shows through from behind.
        webview.isOpaque = false
        webview.backgroundColor = .clear
        webview.scrollView.backgroundColor = .clear
        webview.scrollView.isOpaque = false
        // The AR overlay has nothing worth pinch-zooming as a web page; freeing this up
        // lets our own pinch recognizer (added below) drive camera zoom instead of
        // WKWebView's built-in page-content zoom fighting it for the same gesture.
        webview.scrollView.pinchGestureRecognizer?.isEnabled = false

        let layer = AVCaptureVideoPreviewLayer(session: self.captureSession)
        layer.videoGravity = .resizeAspectFill
        layer.frame = container.bounds
        container.layer.insertSublayer(layer, below: webview.layer)
        self.previewLayer = layer

        if self.pinchGesture == nil {
          let pinch = UIPinchGestureRecognizer(target: self, action: #selector(self.handlePinch(_:)))
          container.addGestureRecognizer(pinch)
          self.pinchGesture = pinch
        }

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

    if let pinch = pinchGesture {
      pinch.view?.removeGestureRecognizer(pinch)
    }
    pinchGesture = nil
    pinchStartZoomFactor = nil
    // Must go before `currentDevice` is cleared: the observation is registered on that
    // device, and emitIntrinsics() reads it.
    zoomObservation?.invalidate()
    zoomObservation = nil
    currentDevice = nil

    webview?.isOpaque = true
    webview?.backgroundColor = nil
    webview?.scrollView.backgroundColor = nil
    webview?.scrollView.pinchGestureRecognizer?.isEnabled = true

    isCameraRunning = false
  }

  @objc private func updatePreviewFrame() {
    guard let container = webview?.superview else { return }
    DispatchQueue.main.async {
      self.previewLayer?.frame = container.bounds
    }
  }

  /// Prefers a virtual multi-lens device (wide + ultrawide/tele) so `videoZoomFactor`
  /// transitions optically between real lenses instead of only digitally cropping a
  /// single wide sensor. Falls back to the plain wide camera on devices/simulators that
  /// don't have one — zoom still works there, just as a digital crop with a smaller
  /// useful range.
  private func backCameraDevice() -> AVCaptureDevice? {
    let preferredTypes: [AVCaptureDevice.DeviceType] = [
      .builtInTripleCamera,
      .builtInDualCamera,
      .builtInDualWideCamera,
      .builtInWideAngleCamera,
    ]
    for type in preferredTypes {
      if let device = AVCaptureDevice.default(type, for: .video, position: .back) {
        return device
      }
    }
    return nil
  }

  //
  // Zoom (pinch gesture)
  //

  @objc private func handlePinch(_ gesture: UIPinchGestureRecognizer) {
    guard let device = currentDevice else { return }

    switch gesture.state {
    case .began:
      pinchStartZoomFactor = device.videoZoomFactor
    case .changed:
      guard let startZoom = pinchStartZoomFactor else { return }
      let minZoom = device.minAvailableVideoZoomFactor
      // Uncapped maxAvailableVideoZoomFactor can be absurd (100x+) on devices that allow
      // arbitrary digital cropping past the point of being useful.
      let maxZoom = min(device.maxAvailableVideoZoomFactor, 10.0)
      let target = max(minZoom, min(maxZoom, startZoom * gesture.scale))

      do {
        try device.lockForConfiguration()
        device.videoZoomFactor = target
        device.unlockForConfiguration()
      } catch {
        Logger.error("Failed to set camera zoom: \(error)")
      }
    default:
      break
    }
  }

  //
  // Photo capture
  //

  /// Captures a real photo via `AVCapturePhotoOutput` and composites the transparent
  /// WKWebView's AR overlay on top of it, saving the result to the Photos library.
  ///
  /// Earlier versions of this tried to snapshot the on-screen `AVCaptureVideoPreviewLayer`
  /// directly (first via `drawHierarchy`, which only walks the real view hierarchy and
  /// can't see a layer manually inserted outside of it; then via `CALayer.render(in:)`,
  /// which for most layers works fine but not for `AVCaptureVideoPreviewLayer`
  /// specifically — its live video content is composited straight to the display via an
  /// IOSurface-backed path that bypasses Core Animation's software render entirely, so
  /// `render(in:)` on it comes back blank). An actual photo capture is the only reliable
  /// way to get real pixels out of the session; the delegate callback below does the
  /// compositing once that photo comes back.
  @objc public func capturePhoto(_ invoke: Invoke) throws {
    guard webview?.superview != nil else {
      invoke.reject("Camera view is not attached to a window yet.")
      return
    }
    guard isCameraRunning else {
      invoke.reject("Camera is not running.")
      return
    }

    if let connection = photoOutput.connection(with: .video), connection.isVideoOrientationSupported {
      // The app's AR viewfinder is used held upright; matches the assumption already
      // made for pitch/roll in startMotionUpdates.
      connection.videoOrientation = .portrait
    }

    pendingCaptureInvoke = invoke
    photoOutput.capturePhoto(with: AVCapturePhotoSettings(), delegate: self)
  }

  /// The destination rect to draw `imageSize` into `boundsSize` with aspect-fill
  /// framing (matching the live preview's `videoGravity = .resizeAspectFill`): scaled up
  /// so it covers the full bounds, centred, with the overflow left for the caller's
  /// graphics context to clip — simpler and safer than manually cropping the source
  /// image's pixel buffer, which is easy to get subtly wrong around orientation.
  private func aspectFillRect(imageSize: CGSize, in boundsSize: CGSize) -> CGRect {
    guard imageSize.width > 0, imageSize.height > 0 else {
      return CGRect(origin: .zero, size: boundsSize)
    }
    let imageAspect = imageSize.width / imageSize.height
    let boundsAspect = boundsSize.width / boundsSize.height

    var drawSize = boundsSize
    if imageAspect > boundsAspect {
      drawSize.width = boundsSize.height * imageAspect
    } else {
      drawSize.height = boundsSize.width / imageAspect
    }
    let origin = CGPoint(x: (boundsSize.width - drawSize.width) / 2, y: (boundsSize.height - drawSize.height) / 2)
    return CGRect(origin: origin, size: drawSize)
  }

  func photoOutput(
    _ output: AVCapturePhotoOutput, didFinishProcessingPhoto photo: AVCapturePhoto, error: Error?
  ) {
    guard let invoke = pendingCaptureInvoke else { return }
    pendingCaptureInvoke = nil

    if let error = error {
      invoke.reject("Photo capture failed: \(error.localizedDescription)")
      return
    }
    guard
      let data = photo.fileDataRepresentation(),
      let cameraImage = UIImage(data: data)
    else {
      invoke.reject("Failed to process captured photo.")
      return
    }

    DispatchQueue.main.async {
      guard let webview = self.webview, let container = webview.superview else {
        invoke.reject("Camera view is not attached to a window yet.")
        return
      }

      let renderer = UIGraphicsImageRenderer(bounds: container.bounds)
      let composited = renderer.image { _ in
        let fillRect = self.aspectFillRect(imageSize: cameraImage.size, in: container.bounds.size)
        cameraImage.draw(in: fillRect)
        webview.drawHierarchy(in: webview.bounds, afterScreenUpdates: true)
      }

      guard let jpegData = composited.jpegData(compressionQuality: 0.92) else {
        invoke.reject("Failed to encode captured photo.")
        return
      }

      PHPhotoLibrary.requestAuthorization(for: .addOnly) { status in
        guard status == .authorized || status == .limited else {
          invoke.reject("Photo library access denied.")
          return
        }

        PHPhotoLibrary.shared().performChanges({
          let request = PHAssetCreationRequest.forAsset()
          request.addResource(with: .photo, data: jpegData, options: nil)
        }) { success, error in
          DispatchQueue.main.async {
            if success {
              invoke.resolve()
            } else {
              invoke.reject(error?.localizedDescription ?? "Failed to save photo.")
            }
          }
        }
      }
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

  //
  // Capture intrinsics (real FOV + zoom, for the AR projection's focal length)
  //

  /// Streams what the capture device reports about its optics. The AR overlay previously
  /// assumed a fixed on-screen horizontal FOV, which is wrong three ways on a phone: the
  /// device's FOV is measured across the buffer's *long* axis (screen height in portrait),
  /// `.resizeAspectFill` crops the width, and pinch zoom narrows both. Reporting the raw
  /// numbers lets the projection derive a real focal length instead of guessing.
  @objc public func startIntrinsicsUpdates(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(StartIntrinsicsArgs.self)

    guard let device = currentDevice else {
      invoke.reject("Camera is not running; start it before requesting intrinsics.")
      return
    }
    self.intrinsicsChannel = args.channel

    // Zoom is user-driven and continuous, so observe the device property rather than
    // emitting from the pinch handler: KVO also catches `ramp(toVideoZoomFactor:)` and
    // the automatic lens transitions a virtual multi-lens device makes on its own.
    zoomObservation = device.observe(\.videoZoomFactor, options: [.new]) { [weak self] _, _ in
      self?.emitIntrinsics()
    }

    emitIntrinsics()
    invoke.resolve()
  }

  @objc public func stopIntrinsicsUpdates(_ invoke: Invoke) throws {
    zoomObservation?.invalidate()
    zoomObservation = nil
    self.intrinsicsChannel = nil
    invoke.resolve()
  }

  private func emitIntrinsics() {
    guard let channel = intrinsicsChannel, let device = currentDevice else { return }

    let format = device.activeFormat
    let dims = CMVideoFormatDescriptionGetDimensions(format.formatDescription)
    let long = Double(max(dims.width, dims.height))
    let short = Double(min(dims.width, dims.height))

    // On the virtual devices `backCameraDevice` prefers, `videoZoomFactor` is defined
    // relative to the widest constituent lens and `videoFieldOfView` reports that same
    // lens's FOV, so scaling the focal length by the zoom factor stays correct across
    // lens switches. Unverified on real multi-lens hardware — if the overlay visibly
    // jumps when zoom crosses a lens transition, that assumption is what broke.
    let reading: JsonObject = [
      "fovDeg": Double(format.videoFieldOfView),
      "zoomFactor": Double(device.videoZoomFactor),
      "bufferLongPx": long,
      "bufferShortPx": short,
      "timestamp": Int(Date().timeIntervalSince1970 * 1000),
    ]

    channel.send(reading)
  }
}

@_cdecl("init_plugin_camera")
func initPlugin() -> Plugin {
  return CameraPlugin()
}
