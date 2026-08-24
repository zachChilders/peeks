//! Regenerates `src/bindings.ts` from the current Tauri command signatures.
//!
//! A standalone binary rather than a step inside `run()` so `pnpm build`/`pnpm dev` can
//! call it ahead of `tsc` without ever opening a window — see `mountain_view_lib::specta_builder`
//! for the single source of truth both this binary and the real app build their command
//! set from.

fn main() {
    mountain_view_lib::export_bindings();
}
