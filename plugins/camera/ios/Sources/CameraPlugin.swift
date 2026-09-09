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

class StartFramesArgs: Decodable {
  let channel: Channel
}

/// What `capturePhoto` resolves with.
///
/// iOS hands an app no file path for a photo it added to the Photos library, so the
/// "file name" here is one the plugin *assigns* at save time rather than one it reads
/// back — see `capturePhoto`. `localIdentifier` is the library's own handle for the
/// created asset and is the only value here that can fetch the photo again; it is
/// optional because it comes from a placeholder that Photos is not obliged to hand back,
/// and `JSONEncoder` simply omits the key when it is nil.
struct CaptureResult: Encodable {
  let fileName: String
  let localIdentifier: String?
}

/// Fold an angle into (-180, 180]. Roll is reported as a signed tilt, so subtracting the
/// interface's rotation from it has to come back on that scale rather than at 270 degrees.
private func wrap180(_ deg: Double) -> Double {
  let wrapped = deg.truncatingRemainder(dividingBy: 360)
  if wrapped > 180 { return wrapped - 360 }
  if wrapped <= -180 { return wrapped + 360 }
  return wrapped
}

class CameraPlugin: Plugin, CLLocationManagerDelegate, AVCapturePhotoCaptureDelegate,
  AVCaptureVideoDataOutputSampleBufferDelegate
{
  private weak var webview: WKWebView?
  private let captureSession = AVCaptureSession()
  private let photoOutput = AVCapturePhotoOutput()
  private var previewLayer: AVCaptureVideoPreviewLayer?
  private var isCameraRunning = false
  private var currentDevice: AVCaptureDevice?
  private weak var pinchGesture: UIPinchGestureRecognizer?
  private var pinchStartZoomFactor: CGFloat?
  private var pendingCaptureInvoke: Invoke?

  /// How far the rendered image is rotated, clockwise in degrees, from the device's own
  /// portrait frame. Subtracted from the gravity-derived roll in `startMotionUpdates` so
  /// what the AR overlay is told is the tilt of the *picture* rather than of the phone —
  /// in landscape those differ by exactly 90 degrees. Written and read on the main queue.
  private var rollOffsetDeg: Double = 0

  /// Builds the name each capture is filed under in the Photos library. Pinned to the
  /// POSIX locale and UTC on purpose: a device set to a non-Gregorian calendar or to
  /// non-Arabic numerals would otherwise render this template into something that is
  /// neither sortable nor recognisable as a date, and the app's own log of these names
  /// would inherit that.
  private static let captureNameFormatter: DateFormatter = {
    let formatter = DateFormatter()
    formatter.locale = Locale(identifier: "en_US_POSIX")
    formatter.timeZone = TimeZone(secondsFromGMT: 0)
    formatter.dateFormat = "yyyyMMdd-HHmmssSSS"
    return formatter
  }()

  private let locationManager = CLLocationManager()
  private var headingChannel: Channel?

  private let motionManager = CMMotionManager()
  private var motionChannel: Channel?
  /// Running integral of rotation about the local vertical, in degrees, and the
  /// `CMDeviceMotion.timestamp` of the sample it was last advanced to. See
  /// `startMotionUpdates` for what this is and why it is integrated here rather than read
  /// off `CMAttitude`.
  private var relativeYawDeg: Double = 0
  private var lastMotionAt: TimeInterval?

  private var intrinsicsChannel: Channel?
  private var zoomObservation: NSKeyValueObservation?

  private let videoOutput = AVCaptureVideoDataOutput()
  private let frameQueue = DispatchQueue(label: "camera.frames", qos: .userInitiated)
  /// Guards `frameChannel` and `lastFrameAt`, which the capture queue reads and the main
  /// queue writes.
  private let frameLock = NSLock()
  private var frameChannel: Channel?
  private var lastFrameAt: CFTimeInterval = 0

  override init() {
    super.init()
    locationManager.delegate = self
  }

  @objc open override func load(webview: WKWebView) {
    self.webview = webview
    // Explicit rather than relying on whatever else in the process happens to have asked
    // for them: without a generator running, `orientationDidChangeNotification` never
    // fires and the whole capture chain silently stays pinned to portrait. UIKit-only, so
    // it goes to the main queue rather than assuming this hook runs there.
    DispatchQueue.main.async {
      UIDevice.current.beginGeneratingDeviceOrientationNotifications()
    }
    NotificationCenter.default.addObserver(
      self,
      selector: #selector(handleOrientationChange),
      name: UIDevice.orientationDidChangeNotification,
      object: nil
    )
  }

  //
  // Orientation
  //
  // Five things have to agree about which way is up: the preview layer, the frames handed
  // to the skyline fitter, the still photo, the compass datum, and the roll angle the AR
  // overlay projects with. They are set from one place for that reason. A preview rotated
  // into landscape while frames still arrive portrait is not a cosmetic mismatch — the
  // skyline detector scans columns, so a frame rotated 90 degrees puts the horizon
  // running *down* it and there is nothing left for the fit to lock onto.

  /// The interface's current orientation, which is the one the UI (and therefore the
  /// overlay's coordinate system) is laid out in. Portrait when there is no window to
  /// ask, which is also the right answer before the view is attached.
  private func currentInterfaceOrientation() -> UIInterfaceOrientation {
    webview?.window?.windowScene?.interfaceOrientation ?? .portrait
  }

  /// `AVCaptureVideoOrientation` shares raw values with `UIInterfaceOrientation`
  /// (portrait 1, upside-down 2, landscapeRight 3, landscapeLeft 4), so this pairing
  /// needs no swap. Note that the same mapping from `UIDeviceOrientation` *is* inverted —
  /// its landscape cases are named for where the home button went, not for where the
  /// interface ended up — which is the usual way this gets written backwards.
  private static func videoOrientation(for ui: UIInterfaceOrientation)
    -> AVCaptureVideoOrientation
  {
    AVCaptureVideoOrientation(rawValue: ui.rawValue) ?? .portrait
  }

  /// Core Location wants a *device* orientation, so this is where the swap above has to
  /// be paid. It matters more than it looks: `trueHeading` is the app's prior for the
  /// skyline fit, whose yaw search only spans +/-20 degrees, so a heading left referenced
  /// to portrait while the phone is in landscape is 90 degrees out and the fitter can
  /// never lock at all.
  private static func headingOrientation(for ui: UIInterfaceOrientation) -> CLDeviceOrientation {
    switch ui {
    case .landscapeLeft: return .landscapeRight
    case .landscapeRight: return .landscapeLeft
    case .portraitUpsideDown: return .portraitUpsideDown
    default: return .portrait
    }
  }

  /// Degrees the interface has been rotated clockwise from portrait, which is also how
  /// far the capture connections above rotate the image. `UIInterfaceOrientation`
  /// `.landscapeLeft` is the device held with its *top* to the right (it pairs with
  /// `UIDeviceOrientation.landscapeRight`), and the gravity-derived roll reads +90 there.
  private static func rollOffsetDeg(for ui: UIInterfaceOrientation) -> Double {
    switch ui {
    case .landscapeLeft: return 90
    case .landscapeRight: return -90
    case .portraitUpsideDown: return 180
    default: return 0
    }
  }

  /// Point everything orientation-dependent at the interface's current orientation.
  /// Idempotent, and safe to call before the camera exists — the connections it cannot
  /// find yet are set by `startCamera`, which calls this once the preview layer is in
  /// place.
  ///
  /// Still on `videoOrientation`, deprecated in iOS 17 in favour of `videoRotationAngle`,
  /// because the replacement's angle convention is not something this can be checked
  /// against without a device — and a rotation applied the wrong way round is worse than
  /// a deprecation warning. Both connections this file already had were set the same way.
  private func applyCaptureOrientation() {
    let ui = currentInterfaceOrientation()
    let video = Self.videoOrientation(for: ui)

    rollOffsetDeg = Self.rollOffsetDeg(for: ui)
    locationManager.headingOrientation = Self.headingOrientation(for: ui)

    for connection in [
      previewLayer?.connection,
      videoOutput.connection(with: .video),
      photoOutput.connection(with: .video),
    ] {
      guard let connection = connection, connection.isVideoOrientationSupported else { continue }
      connection.videoOrientation = video
    }
  }

  @objc private func handleOrientationChange() {
    // Deferred: the notification is posted off the *device* orientation, which moves
    // before the interface finishes rotating, so reading the window scene or the
    // container's bounds synchronously here can still return the outgoing layout.
    DispatchQueue.main.async {
      if let container = self.webview?.superview {
        self.previewLayer?.frame = container.bounds
      }
      self.applyCaptureOrientation()
    }
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
          // Frames for skyline fitting. The Y plane of a biplanar YCbCr buffer *is* the
          // grayscale image, so requesting this format means no colour conversion at all.
          self.videoOutput.videoSettings = [
            kCVPixelBufferPixelFormatTypeKey as String:
              kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
          ]
          // Fitting wants a recent frame, not every frame; dropping is correct here.
          self.videoOutput.alwaysDiscardsLateVideoFrames = true
          self.videoOutput.setSampleBufferDelegate(self, queue: self.frameQueue)
          if self.captureSession.canAddOutput(self.videoOutput) {
            self.captureSession.addOutput(self.videoOutput)
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

        // Now that the preview layer exists there is a full set of connections to point
        // at the current orientation — including the frame output, which has to deliver a
        // gravity-upright buffer for the skyline detector's column scan to mean anything.
        self.applyCaptureOrientation()

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

    // Stop delivery before dropping the channel, so an in-flight frame on the capture
    // queue can't reach a torn-down consumer.
    videoOutput.setSampleBufferDelegate(nil, queue: nil)
    frameLock.lock()
    frameChannel = nil
    frameLock.unlock()

    webview?.isOpaque = true
    webview?.backgroundColor = nil
    webview?.scrollView.backgroundColor = nil
    webview?.scrollView.pinchGestureRecognizer?.isEnabled = true

    isCameraRunning = false
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

    // The overlay is composited onto this photo at the container's own bounds, so the
    // photo has to come back in the same orientation the container is laid out in —
    // otherwise a landscape capture arrives portrait and aspect-fill crops most of it
    // away to cover the frame.
    applyCaptureOrientation()

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

      // Named here rather than left to Photos, which would otherwise file the asset
      // under a generic name of its own. The app records this name against the
      // capture's coordinates, and a name it chose is the only handle it can be sure
      // matches — there is no path to read back for a library asset.
      let fileName = "Peeks-\(CameraPlugin.captureNameFormatter.string(from: Date())).jpg"

      PHPhotoLibrary.requestAuthorization(for: .addOnly) { status in
        guard status == .authorized || status == .limited else {
          invoke.reject("Photo library access denied.")
          return
        }

        // Written inside the change block and read in the completion handler below.
        // Photos runs the two in order on its own queue, and the placeholder's
        // identifier is the created asset's, so it is valid as soon as the change
        // commits.
        var localIdentifier: String?

        PHPhotoLibrary.shared().performChanges({
          let request = PHAssetCreationRequest.forAsset()
          let options = PHAssetResourceCreationOptions()
          options.originalFilename = fileName
          request.addResource(with: .photo, data: jpegData, options: options)
          localIdentifier = request.placeholderForCreatedAsset?.localIdentifier
        }) { success, error in
          DispatchQueue.main.async {
            if success {
              invoke.resolve(CaptureResult(fileName: fileName, localIdentifier: localIdentifier))
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

    // Both sensor streams start on the landing page, before there is a camera, so both
    // seed the orientation rather than leaving it to `startCamera`: an app *opened* in
    // landscape would otherwise report a heading referenced to portrait, and a roll
    // referenced to the phone, until it was next rotated.
    applyCaptureOrientation()
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
  // Device motion (pitch/roll and relative heading, for a phone held upright as an AR
  // viewfinder)
  //

  @objc public func startMotionUpdates(_ invoke: Invoke) throws {
    guard motionManager.isDeviceMotionAvailable else {
      invoke.reject("Device motion is not available on this device.")
      return
    }

    let args = try invoke.parseArgs(StartMotionArgs.self)
    self.motionChannel = args.channel
    self.relativeYawDeg = 0
    self.lastMotionAt = nil

    // See `startHeadingUpdates`: the roll reported below is referenced to the interface,
    // so `rollOffsetDeg` has to be right before the first sample arrives. Not merely a
    // duplicate of the call there — heading rejects outright on a device with no
    // magnetometer, and this stream still runs.
    applyCaptureOrientation()

    motionManager.deviceMotionUpdateInterval = 1.0 / 30.0
    // `.xArbitraryZVertical` explicitly rather than by default: Z is the true vertical
    // (gravity-referenced, so it does not drift), and the heading origin is arbitrary and
    // magnetometer-free. That is exactly the frame `relativeYawDeg` below wants — the app
    // gets its absolute heading from the skyline fit, and asking CoreMotion for a
    // magnetically-corrected frame would feed the compass back in through the side door.
    motionManager.startDeviceMotionUpdates(using: .xArbitraryZVertical, to: .main) { motion, error in
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
      //   roll  = rotation of the rendered image about that axis (0 = image upright)
      // Unverified on real hardware — Simulator has no motion sensors to check against.
      // If pitch or roll reads inverted on a real device, flip the corresponding sign.
      let g = motion.gravity
      let gLen = (g.x * g.x + g.y * g.y + g.z * g.z).squareRoot()

      // The elevation of the optical axis is the gravity component along it and nothing
      // else, which is why this is an `asin` and not an `atan2` against some second axis.
      // It was `atan2(g.z, -g.y)` — correct only while -Y was still the up direction,
      // i.e. in portrait. Turn the phone to landscape and -g.y goes to ~0 alongside g.z,
      // leaving atan2(noise, noise): pitch swung to +/-90 with the camera aimed level at
      // a ridge, which is what made the overlay unusable there.
      let pitch = gLen > 0 ? asin(max(-1.0, min(1.0, g.z / gLen))) * 180.0 / .pi : 0

      // The phone's own roll, minus however far the interface has turned to stay upright.
      // The overlay is drawn in the interface's coordinate system and projected against
      // an image the capture connections have rotated to match it, so what both want is
      // the residual tilt of the picture: ~0 in portrait and in landscape alike, and only
      // non-zero when the phone is genuinely held off-square.
      let roll = wrap180(atan2(g.x, -g.y) * 180.0 / .pi - self.rollOffsetDeg)

      // Rotation about the local vertical, integrated. This is the *change* in compass
      // heading with no notion of where north is: an arbitrary but stable datum the app
      // pins to true north once, using the skyline fit, instead of re-reading a
      // magnetometer that is routinely several degrees off near a car or a magnetic case.
      //
      // Integrated from `rotationRate` rather than read off `CMAttitude` deliberately.
      // Both `rotationRate` and `gravity` are documented as vectors in the *device* frame,
      // so this needs no assumption about which direction `CMAttitude.rotationMatrix`
      // maps — a convention that is easy to get backwards and impossible to check without
      // hardware. It also sidesteps the Euler-angle degeneracy that already forced pitch
      // and roll above to come from gravity: held upright, a phone sits at the gimbal lock
      // of `attitude.yaw`. `rotationRate` is the bias-corrected rate (unlike
      // `CMGyro.rotationRate`), so the dominant drift term is already removed by
      // CoreMotion; what is left accumulates slowly and is what re-fitting corrects.
      //
      // `motion.timestamp` is the sample's own clock, so a dropped or late callback
      // integrates the interval it actually covers rather than a nominal 1/30s.
      if let last = self.lastMotionAt, gLen > 0 {
        let dt = motion.timestamp - last
        // A gap this long means the stream stalled (backgrounded, say); integrating
        // across it would invent rotation that may never have happened, so the datum
        // simply holds and the next fit corrects whatever was missed.
        if dt > 0, dt < 1.0 {
          // Up in device coordinates is -gravity, whatever way the phone is being held.
          let up = (x: -g.x / gLen, y: -g.y / gLen, z: -g.z / gLen)
          let r = motion.rotationRate
          // Right-hand rule about up is counter-clockwise seen from above; compass
          // azimuth runs the other way, hence the subtraction.
          let rateAboutUp = r.x * up.x + r.y * up.y + r.z * up.z
          self.relativeYawDeg -= rateAboutUp * dt * 180.0 / .pi
        }
      }
      self.lastMotionAt = motion.timestamp

      let reading: JsonObject = [
        "pitch": pitch,
        "roll": roll,
        "relativeYawDeg": self.relativeYawDeg,
        "timestamp": Int(Date().timeIntervalSince1970 * 1000),
      ]
      self.motionChannel?.send(reading)
    }

    invoke.resolve()
  }

  @objc public func stopMotionUpdates(_ invoke: Invoke) throws {
    motionManager.stopDeviceMotionUpdates()
    self.motionChannel = nil
    self.lastMotionAt = nil
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

  //
  // Frames (for skyline fitting)
  //

  /// Target size of the downsampled frame's *short* axis. Small enough to be cheap to
  /// ship and process, large enough that quantising the skyline to whole rows stays under
  /// about a quarter of a degree — see `peakcore::skyline::fit`, whose accuracy is floored
  /// by exactly this.
  ///
  /// The short axis rather than the width, because the frame is landscape once the phone
  /// is: fixing the width would shrink a landscape frame to 160 px across the *long* axis
  /// and cost most of that angular resolution just for turning the phone.
  private static let frameShortPx = 160
  /// Fitting wants a recent frame, not a fast one. Everything else is dropped.
  private static let frameInterval: CFTimeInterval = 0.5

  @objc public func startFrameUpdates(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(StartFramesArgs.self)
    frameLock.lock()
    frameChannel = args.channel
    frameLock.unlock()
    invoke.resolve()
  }

  @objc public func stopFrameUpdates(_ invoke: Invoke) throws {
    frameLock.lock()
    frameChannel = nil
    frameLock.unlock()
    invoke.resolve()
  }

  func captureOutput(
    _ output: AVCaptureOutput,
    didOutput sampleBuffer: CMSampleBuffer,
    from connection: AVCaptureConnection
  ) {
    frameLock.lock()
    let channel = frameChannel
    let since = CACurrentMediaTime() - lastFrameAt
    if channel != nil && since >= Self.frameInterval {
      lastFrameAt = CACurrentMediaTime()
    }
    frameLock.unlock()

    guard let channel = channel, since >= Self.frameInterval else { return }
    guard let pixels = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }

    CVPixelBufferLockBaseAddress(pixels, .readOnly)
    defer { CVPixelBufferUnlockBaseAddress(pixels, .readOnly) }

    guard let base = CVPixelBufferGetBaseAddressOfPlane(pixels, 0) else { return }
    let srcW = CVPixelBufferGetWidthOfPlane(pixels, 0)
    let srcH = CVPixelBufferGetHeightOfPlane(pixels, 0)
    let stride = CVPixelBufferGetBytesPerRowOfPlane(pixels, 0)
    guard srcW > 0, srcH > 0 else { return }

    let scale = Double(Self.frameShortPx) / Double(min(srcW, srcH))
    let dstW = max(1, min(srcW, Int((Double(srcW) * scale).rounded())))
    let dstH = max(1, min(srcH, Int((Double(srcH) * scale).rounded())))

    // Box-average, matching `peakcore::skyline::downsample_gray`. Point-sampling would
    // alias thin bright features (a lit cloud edge, a snow patch) into the brightness
    // step the detector keys on, and a config tuned on desktop would stop transferring.
    let src = base.assumingMemoryBound(to: UInt8.self)
    var out = [UInt8](repeating: 0, count: dstW * dstH)
    for dy in 0..<dstH {
      let y0 = dy * srcH / dstH
      let y1 = max(y0 + 1, min(srcH, (dy + 1) * srcH / dstH))
      for dx in 0..<dstW {
        let x0 = dx * srcW / dstW
        let x1 = max(x0 + 1, min(srcW, (dx + 1) * srcW / dstW))
        var sum = 0
        var n = 0
        for y in y0..<y1 {
          let row = src + y * stride
          for x in x0..<x1 {
            sum += Int(row[x])
            n += 1
          }
        }
        out[dy * dstW + dx] = UInt8(sum / max(n, 1))
      }
    }

    let reading: JsonObject = [
      "width": dstW,
      "height": dstH,
      "gray": Data(out).base64EncodedString(),
      "timestamp": Int(Date().timeIntervalSince1970 * 1000),
    ]
    channel.send(reading)
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
