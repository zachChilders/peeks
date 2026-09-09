# Tauri + React + Typescript

This template should help get you started developing with Tauri, React and Typescript in Vite.

## App flow

Three screens, switched by `App.tsx`: a landing page, the topo map, and the AR camera
view. The landing page is the entry point and exists for two reasons — choosing between
the map and the camera, and getting the orientation pipeline started before the camera is
opened rather than after.

`src/lib/orientation.ts` owns that pipeline and lives in module scope, outside React, so it
survives every navigation between views. It fixes a position, samples the DEM for ground
elevation, resolves the visible peaks and the terrain horizon into Rust (`set_scene` /
`set_horizon`), and starts the compass and gyro streams. None of that needs the camera; all
of it used to run on `CameraView` mount, which is why opening the camera meant watching
"Orienting…" through the whole sequence.

`CameraView` is left with the parts that genuinely need the capture device: the preview,
its intrinsics, the skyline fitter's frame stream (`start_calibration`), and the 100ms
projection tick. It re-checks the prewarmed position on mount and rebuilds the scene if the
observer has moved far enough for peak visibility or the horizon to have changed.

The on-device pipeline trace (`src/lib/debugLog.ts`) is shared by every view and shown in a
drawer that is collapsed by default — most of its lines are now written before the camera
view exists.

## Orientation

The camera view works in portrait and landscape, and that is not free: five things have to
agree about which way is up, or the overlay is wrong in a way that looks like a different
bug each time. `applyCaptureOrientation` in `plugins/camera/ios/Sources/CameraPlugin.swift` is
the single place that sets them — the preview layer, the frames the skyline fitter reads,
the still photo, Core Location's heading reference, and the roll angle handed to the
projection.

Three of those five had been pinned to portrait, each with its own symptom:

- The **preview layer's** connection stayed portrait while its frame was resized to a
  landscape container, so the image itself came out sideways.
- The **fitter's frames** stayed portrait too, which is worse than cosmetic: the skyline
  detector scans columns, so a frame rotated 90° has the horizon running *down* it and
  there is nothing to lock onto. That is "it tries to fit it portrait".
- **`headingOrientation`** stayed portrait, putting the compass 90° out. The compass is
  only a prior, but the yaw search spans ±20°, so a prior that far off means no fit ever
  succeeds and the app never gets off the compass at all.

Pitch was separately wrong off-portrait. It came from `atan2(g.z, -g.y)`, which is the
right answer only while `-Y` is still the up direction; turn the phone and `-g.y` goes to
~0 alongside `g.z`, leaving `atan2` of two numbers that are both noise. It is an `asin` of
the gravity component along the optical axis now, which needs no assumption about how the
phone is held.

Roll is reported as the tilt of the *picture*, not of the phone — the interface's own
rotation is subtracted out — so it stays ~0 in both orientations and is non-zero only for
a genuine tilt. Everything downstream of the plugin then needs no orientation logic of its
own: `CameraView` re-reads `window.innerWidth/innerHeight` on each tick, and
`CameraIntrinsics::focal_px` pairs the buffer's long axis with the screen's long axis
rather than with width.

Two consequences worth knowing. The on-screen field of view *swaps* rather than staying
put: `resizeAspectFill` crops the horizontal axis in portrait (68° native → ~35° across)
and the vertical one in landscape (~35° down, the full 68° across). And the frame handed
to the fitter is scaled on its short axis, so a landscape frame is 284×160 where a
portrait one is 160×284 — same angular resolution per pixel either way, which is what
`peakcore::skyline::fit`'s accuracy is floored by.

## The capture log

Every photo taken from the camera view is recorded in a SQLite database — one row per
capture, holding the name the photo was filed under in the Photos library and the GPS
position it was taken from (plus the altitude and horizontal accuracy the fix carried, and
the Photos asset identifier that can fetch the picture itself back).

The database is `photos.sqlite3` under the app data directory, which resolves on iOS to
`<container>/Library/Application Support/com.mountainview.app/`. That location is the
point: **iCloud Backup takes everything in the app container except `Library/Caches` and
`tmp`**, so the log rides along in the device backup and survives a restore onto a new
phone. Note the contrast with the DEM tile cache (`src-tauri/src/dem.rs`), which is
deliberately under `app_cache_dir()` — `Library/Caches` — because it is re-downloadable and
has no business inflating a backup. The capture log is not: once the shutter has fired, the
position that photo was taken from exists nowhere else.

Two things about the filename are worth knowing. iOS hands an app no file path for a photo
it saved to the Photos library, so the plugin *assigns* the name at save time
(`Peeks-<UTC yyyyMMdd-HHmmssSSS>.jpg`, via the asset resource's `originalFilename`) rather
than reading one back — see `capturePhoto` in `plugins/camera/ios/Sources/CameraPlugin.swift`.
And the coordinates come from a fix taken at the shutter, not from the AR scene's observer:
the scene is allowed to lag the phone by up to `OBSERVER_STALE_M` (`src/lib/orientation.ts`),
which is the right tolerance for deciding which peaks are visible and the wrong one for
saying where a picture was taken.

Reading the log back is not wired up to any UI — the rows exist to be recovered from a
backup. Timestamps are epoch milliseconds, so `datetime(captured_at_ms / 1000, 'unixepoch')`
is what renders them:

```sh
sqlite3 photos.sqlite3 \
  "SELECT datetime(captured_at_ms / 1000, 'unixepoch'), file_name, lat, lon FROM photos;"
```

## The bundled peak dataset

`src-tauri/resources/peaks.mvpk` holds named `natural=peak` nodes from OpenStreetMap,
bucketed into the same 1°×1° tiles as the DEM. The app reads it instead of querying
Overpass, so peaks resolve with no network — which is the point, since this app gets used
where there is no signal.

**Current scope is North America** — 125,692 peaks, 3.7 MB, covering Alaska through
Panama plus Greenland. Outside that box the app finds no peaks at all; that is an empty
result, not an error.

Regenerate with:

```sh
# North America, ~25 minutes
cargo run --release -p peaklab -- extract-peaks --bbox 5,-172,84,-40

# Whole globe: ~690k peaks, ~20 MB, several hours
cargo run --release -p peaklab -- extract-peaks
```

Each cell's response is cached under `$PEAKLAB_DATA/extract`, so an interrupted run
resumes. Every run snaps to one global cell grid, so widening the region later reuses the
cells already fetched rather than refetching them.

**Run it rarely.** The output is committed, so every regeneration adds its full size to
git history permanently. OSM peak data changes on the order of months, not days.

`committed_dataset_is_real` in `src-tauri/src/peaks.rs` guards against shipping a
truncated or placeholder file — the Tauri build script only checks that the resource
exists, not that it contains anything.

The data is ODbL — bundling it is redistribution, which is why `CameraView` carries an
OpenStreetMap attribution line.

## Recommended IDE Setup

- [VS Code](https://code.visualstudio.com/) + [Tauri](https://marketplace.visualstudio.com/items?itemName=tauri-apps.tauri-vscode) + [rust-analyzer](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer)
