const COMMANDS: &[&str] = &[
  "start_camera",
  "stop_camera",
  "start_heading_updates",
  "stop_heading_updates",
  "start_motion_updates",
  "stop_motion_updates",
];

fn main() {
  tauri_plugin::Builder::new(COMMANDS)
    .android_path("android")
    .ios_path("ios")
    .build();
}
