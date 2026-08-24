# Refactor: single Rust core, zero hand-ported TypeScript

## Goal

Delete `src/lib/geo.ts`, `src/lib/projection.ts`, and the hand-written parts of
`src/lib/peaks.ts`. All geodesy, projection, label layout, and remote-data fetching
lives in Rust, shared by `peaklab` and `src-tauri`. TypeScript keeps only React
components, DOM rendering, and canvas text measurement (a browser API with no Rust
equivalent).

**Success condition:** `grep -r "Direct port of" src/` returns nothing, and every type
crossing the IPC boundary is generated from Rust, not typed by hand.

---

## Target layout

```
Cargo.toml              # NEW: [workspace] root, shared profiles + dep versions
crates/peakcore/        # NEW: geo, projection, overpass query/parse. No I/O.
peaklab/                # keeps dem, visibility, render, main; depends on peakcore
src-tauri/              # keeps commands; depends on peakcore
plugins/{camera,barometer}/
src/                    # React only + generated bindings
```

`peakcore` is **transport-free on purpose.** `peaklab` uses `reqwest` 0.12 blocking;
`src-tauri` uses 0.13.4 async. Rather than force a version unification through a shared
HTTP layer, `peakcore` exposes `overpass::build_query(lat, lon, radius) -> String` and
`overpass::parse_response(&str) -> Vec<RawPeak>`, and each caller owns its own
transport. Same dedup benefit, no dependency conflict.

---

## Phase 0 — Workspace and lockfile hygiene

No behavior change. Land and verify this alone before touching any code.

Create the root `Cargo.toml`:

```toml
[workspace]
members = ["crates/peakcore", "peaklab", "src-tauri", "plugins/*"]
resolver = "2"

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[profile.release]
debug = true
```

Four things that will bite here:

1. **`peaklab/Cargo.toml` currently declares `[profile.release] debug = true`.** Cargo
   ignores profiles in non-root workspace members and emits a warning. Move it to the
   root as shown and delete it from `peaklab`.
2. **`plugins/*` become workspace members whether you list them or not** — path
   dependencies residing inside the workspace directory are auto-included. Listing them
   explicitly is clearer. Consequence: their individual `Cargo.lock` files stop being
   used, so remove the `Cargo.lock` line from `plugins/*/.gitignore`.
3. **`target/` moves to the workspace root.** Update `src-tauri/.gitignore` (`/target`)
   and `peaklab/.gitignore` (`/target`) to point at the root, and confirm Tauri's iOS
   `preBuildScript` (`pnpm tauri ios xcode-script`) still resolves `libapp.a` — it reads
   the target dir from cargo metadata, so it should, but this is the step most likely to
   surprise you.
4. **Commit the root `Cargo.lock`.** Neither binary crate currently tracks one; a
   workspace gives you exactly one, and it should be in git.

**Gate:** `cargo test --workspace` passes (14 tests), `pnpm tauri ios build --debug`
completes.

---

## Phase 1 — Extract `peakcore`

Move, don't rewrite:

| From | To | Tests moved |
|---|---|---|
| `peaklab/src/geo.rs` | `crates/peakcore/src/geo.rs` | 6 |
| `peaklab/src/projection.rs` | `crates/peakcore/src/projection.rs` | 4 |
| Overpass query/parse from `peaklab/src/peaks.rs` + `src-tauri/src/overpass.rs` | `crates/peakcore/src/overpass.rs` | 1 (`ele_parsing`) |

`peaklab/src/lib.rs` re-exports so its own modules and `main.rs` keep compiling
unchanged:

```rust
pub use peakcore::{geo, projection};
```

`crates/peakcore/Cargo.toml` stays dependency-light — `serde` only. No `reqwest`, no
`anyhow` (use `Result<_, ParseError>` with `thiserror`), no `image`. That keeps it cheap
to compile into the iOS static lib.

Keep `layout_labels`' signature exactly as it is — generic over
`measure: impl Fn(&str) -> (f64, f64)`. `peaklab` keeps passing its `ab_glyph` closure;
the Tauri command will pass a `HashMap` lookup closure (see Phase 3). No signature
change needed, which is what makes this a pure move.

`src-tauri/src/overpass.rs` shrinks to transport only — it calls
`peakcore::overpass::build_query` and `parse_response`, and the hand-rolled
`urlencoding_query` helper dies (use `reqwest`'s `.form(&[("data", q)])`, same as
`peaklab` already does).

**Gate:** `cargo test --workspace` — same 14 tests, same results, now split across two
crates.

---

## Phase 2 — Generated bindings

This is the phase that actually enforces "no hand-ported TS," so don't skip it and
hand-write the interface types.

Use **`tauri-specta` v2**, which generates both the TypeScript types *and* typed invoke
wrappers from the Rust command signatures:

```rust
#[derive(Serialize, Type)]
pub struct PlacedLabel { name: String, anchor: (f64, f64), rect: Option<Rect> }

#[tauri::command]
#[specta::specta]
fn project_labels(pose: CameraPose, state: State<Scene>) -> Vec<PlacedLabel> { ... }
```

Emit to `src/bindings.ts`, gitignore it, and generate it in a `build:bindings` script
that runs ahead of `tsc` — the same ordering problem that currently makes `pnpm build`
fail on a clean checkout, since nothing builds the plugins' `dist-js` first. Wire both
into the root `build` script:

```json
"build:bindings": "cargo run -p mountain-view --bin generate-bindings",
"build:plugins": "pnpm -r --filter './plugins/*' build",
"build": "pnpm build:plugins && pnpm build:bindings && tsc && vite build",
"dev": "pnpm build:plugins && pnpm build:bindings && vite"
```

Fallback if `tauri-specta` fights Tauri 2.11: `ts-rs` with `#[derive(TS)] #[ts(export)]`
gives you the types on `cargo test`, but you write the `invoke()` calls by hand.
Acceptable — types are where drift actually hurts.

---

## Phase 3 — Move the projection tick into Rust

The design question here is the `measure` callback: `layoutLabels` needs pixel text
widths, which only the browser can produce. Rust can't call back into JS mid-computation.

**Resolution:** peak names are fixed for the lifetime of a scene, so measure them once
in JS at load time and ship the metrics with the peaks. Two commands, not one:

```rust
// Called once per observer position.
#[tauri::command]
fn set_scene(observer: Geodetic, peaks: Vec<PeakWithMetrics>, state: State<Scene>);

// Called every tick (100ms).
#[tauri::command]
fn project_labels(pose: CameraPose, state: State<Scene>) -> Vec<PlacedLabel>;
```

`set_scene` precomputes, once: the ENU vector per peak, the great-circle distance per
peak, and the distance sort. `project_labels` then does one `basis()` plus N dot
products, a cull, and the layout.

**This is a real performance win, not just tidiness.** The current TS recomputes
`cameraBasis()` inside `project()` for *every peak on every tick*
(`projection.ts:75`), and calls `enu()` twice per peak per tick — once for projection,
once for `greatCircleDistance` (`CameraView.tsx:175,185`). Each `enu()` is two ECEF
conversions, eight trig calls. At 10Hz with a few hundred peaks that's tens of thousands
of redundant `sin`/`cos` per second. After this phase, all of it collapses to a one-time
cost at `set_scene`.

The Tauri command builds its measure closure from the shipped metrics:

```rust
let widths: HashMap<&str, (f64, f64)> = /* from scene */;
let placed = layout_labels(&candidates, |name| widths[name], 6, 4.0);
```

JS side, in `CameraView`:

```ts
await document.fonts.ready;          // don't measure before the font loads
const metrics = peaks.map(p => ({ ...p, ...measure(p.name) }));
await commands.setScene(observer, metrics);
```

Two correctness notes to carry in while rewriting this path:

- The `await document.fonts.ready` gate is new and necessary — measuring against a
  fallback font before `-apple-system` resolves gives wrong widths, and now those wrong
  widths get cached in Rust rather than recomputed each tick.
- Key the returned labels on `osmId`, not `name`. Duplicate summit names are common in
  OSM ("Bald Mountain", "Black Butte"), and `CameraView.tsx:229` currently uses
  `key={label.name}`, which yields duplicate React keys. `set_scene` should carry
  `osmId` through to `PlacedLabel`.

Then delete `src/lib/geo.ts` and `src/lib/projection.ts` outright.

**Gate:** measure the IPC round-trip before committing. Log `performance.now()` around
`project_labels` for 100 ticks. Expect well under 1ms for a few hundred labels; if it
exceeds ~5ms, fall back to keeping projection in TS but generating it from Rust via WASM
rather than reverting to a hand port. Record the number in a comment so the next person
doesn't have to re-derive it — the *absence* of this measurement is exactly why the
current TS port exists.

---

## Phase 4 — Fold the networking in

`src/lib/peaks.ts` currently does Overpass-via-`invoke` plus a batched Open-Elevation
fetch in TS, and `fetchElevation` is duplicated verbatim in `MapView.tsx:32` and
`CameraView.tsx:36`. Collapse all of it:

```rust
#[tauri::command] async fn fetch_peaks(lat, lon, radius_m) -> Result<Vec<Peak>, Error>;
#[tauri::command] async fn get_elevation(lat, lon) -> Result<f64, Error>;
```

`fetch_peaks` does Overpass -> filter named -> batched Open-Elevation -> assemble, all in
Rust. While moving it, fix two defects in the code being replaced:

- **Validate batch lengths.** `peaks.ts:60` pre-fills the output array with `0` and only
  writes indices the API actually returned, so a short response silently leaves peaks at
  sea level instead of erroring.
- **Set a request timeout.** `src-tauri/src/overpass.rs:16` builds its client with none,
  so a hung Overpass pins the AR view at "Orienting..." forever. Mirror `peaklab`'s 180s.

`src/lib/peaks.ts` is deleted; call sites use the generated `commands.fetchPeaks`.

---

## Phase 5 — Parity verification

The whole point of `peaklab` was that its math is tested. Prove the shared crate didn't
change behavior:

1. `cargo test --workspace` — the 10 moved geo/projection tests must pass unchanged in
   their new home.
2. **Golden-file parity:** run `peaklab render` against a fixed lat/lon/yaw before and
   after the extraction; the output PNGs must be byte-identical. This is the strongest
   single check that the move was pure.
3. **Add the tests the TS port never had.** Now that there's one implementation,
   `straight_ahead_projects_to_image_center`, `behind_camera_is_none`,
   `point_right_of_center_projects_to_positive_x`, and
   `overlapping_labels_stack_instead_of_colliding` cover the shipping mobile path too —
   which they previously did not.
4. `pnpm tauri ios build` on a clean clone (`git clean -xfd` in a scratch copy),
   confirming the build-ordering fix from Phase 2 holds.

---

## Risks

| Risk | Mitigation |
|---|---|
| Tauri iOS build breaks on the workspace target-dir move | Phase 0 lands alone and is verified with a real `ios build` before anything else moves |
| `peakcore` pulls heavy deps into the iOS static lib | Hard rule: `serde` + `thiserror` only; no `reqwest`/`image`/`anyhow` |
| IPC at 10Hz too slow | Measured in Phase 3 with an explicit fallback (WASM, not hand-port) |
| `tauri-specta` incompatible with Tauri 2.11.3 | Fall back to `ts-rs` for types, hand-write only the `invoke()` calls |
| Text metrics wrong if font loads late | `await document.fonts.ready` before `set_scene` |

## Explicitly out of scope

Keeping this refactor behavior-preserving so the golden-file check stays meaningful.
These land separately:

- The `HFOV_DEG = 63` error in `CameraView.tsx:32` — it's a value fix, and changing it
  mid-refactor would invalidate the parity comparison.
- Interface-rotation handling, the `startCamera` preview-layer leak, and the `Blend` fix
  for `render.rs` leader lines.
- Porting `dem.rs`/`visibility.rs` to mobile. That's the one *legitimate* divergence —
  you can't ship Copernicus tiles in a phone app — and it needs its own design
  (server-side visibility service, or a precomputed horizon profile per region).

---

## Order of landing

Five commits, each green on its own:

1. `chore: cargo workspace` — Phase 0
2. `refactor: extract peakcore crate` — Phase 1, golden-file verified
3. `build: generate TS bindings from Rust` — Phase 2
4. `refactor: move projection tick to Rust, delete geo.ts/projection.ts` — Phase 3
5. `refactor: move peak/elevation fetching to Rust, delete peaks.ts` — Phase 4
