# Tauri + React + Typescript

This template should help get you started developing with Tauri, React and Typescript in Vite.

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
