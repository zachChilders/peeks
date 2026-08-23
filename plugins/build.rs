const COMMANDS: &[&str] = &["is_available", "start_updates", "stop_updates"];

fn main() {
  tauri_plugin::Builder::new(COMMANDS)
    .android_path("android")
    .ios_path("ios")
    .build();
}
