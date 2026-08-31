use specta_typescript::Typescript;
use tauri_specta::{collect_commands, Builder};

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
#[specta::specta]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

mod calibration;
mod dem;
mod peaks;
mod peakstore;
mod scene;

/// The single source of truth for which commands exist and what they look like.
/// `run()` and the `generate-bindings` binary both build from this, so the app's
/// `invoke_handler` and `src/bindings.ts` can never drift apart.
fn specta_builder() -> Builder<tauri::Wry> {
    Builder::<tauri::Wry>::new().commands(collect_commands![
        greet,
        peaks::fetch_peaks,
        peaks::get_elevation,
        peaks::filter_visible_peaks,
        peaks::compute_horizon,
        scene::set_scene,
        scene::set_horizon,
        scene::project_labels,
        calibration::start_calibration,
        calibration::stop_calibration,
    ])
}

/// Writes `src/bindings.ts` from the current command signatures. Called by the
/// `generate-bindings` binary (`pnpm build:bindings`), never at app runtime.
///
/// Anchored on `CARGO_MANIFEST_DIR` rather than a relative `"../src/bindings.ts"`: the
/// binary is invoked via `pnpm` from the repo root, not from `src-tauri`, so a
/// CWD-relative path would land outside the repo entirely.
pub fn export_bindings() {
    let out = concat!(env!("CARGO_MANIFEST_DIR"), "/../src/bindings.ts");
    specta_builder()
        .export(Typescript::default(), out)
        .expect("failed to export typescript bindings");
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = specta_builder();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_geolocation::init())
        .plugin(tauri_plugin_barometer::init())
        .plugin(tauri_plugin_camera::init())
        .manage(scene::Scene::default())
        .manage(dem::DemCache::default())
        .manage(peakstore::PeakStore::default())
        .manage(calibration::Calibration::default())
        .invoke_handler(builder.invoke_handler())
        .setup(move |app| {
            builder.mount_events(app);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
