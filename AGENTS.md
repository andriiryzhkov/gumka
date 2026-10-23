# AGENTS.md

Instructions for AI coding agents working in this repository. Read this file first, then the
documents in [`docs/`](docs/README.md) that cover the area you are changing.

## Local overrides

If `AGENTS.local.md` exists in the repository root, read it too and follow it: it holds this
machine's local overrides, and where it conflicts with this file, it wins. It is git-ignored and
never committed, so don't add it to the repository, don't edit it unless asked, and don't move
its contents into tracked files.

## The project

Gumka is an extensible AI photo-retouching app: a native Rust + egui desktop app that erases
objects with LaMa, selects them with SAM 2.1 and upscales with RealPLKSR, on-device through Core
ML, working on 16-bit, wide-gamut TIFF / PNG / JPEG files without losing their metadata. It runs
standalone and as the external editor of RAW editors (any that send a file and pick it up when it
is saved in place). macOS 13+ on Apple Silicon is the working platform; Windows and Linux must
keep building but have no model backend yet. License: GPL-3.0-or-later.
Current status and scope: [docs/overview.md](docs/overview.md).

## Repository map

| Path | What it is |
|---|---|
| `crates/gumka-core` | Pictures, TIFF/PNG/JPEG I/O and metadata, color, erase, select and upscale pipelines. No UI, no ML. |
| `crates/gumka-infer` | Model registry and the Core ML backend (LaMa, SAM, RealPLKSR). |
| `crates/gumka-cli` | `gumka-cli erase`, `select` and `upscale`. |
| `crates/gumka-app` | The egui app (binary `gumka`). |
| `xtask` | `cargo xtask models` (download and verify the models), `cargo xtask bundle` (the macOS app). |
| `models.toml` | The model registry: pinned URLs, sizes, SHA-256, licenses. |
| `rust-toolchain.toml` | The pinned Rust version, used locally and in CI. |
| `models/` | Downloaded models (git-ignored). |
| `tools/convert` | Python scripts (uv) that convert the models we convert ourselves; run by hand, not part of the build. |
| `.github/workflows` | CI on pull requests, the nightly build on pushes to `main`, releases on `release-*` tags. |
| `assets/logo` | The logo SVGs. |
| `docs/` | Developer documentation. `README.md` at the root is for users. |

Module-level detail: [docs/architecture.md](docs/architecture.md).

## Commands

```sh
cargo xtask models                                   # once: download the models (~340 MB)
cargo run --release -p gumka-app [-- IMAGE]          # the app
cargo run --release -p gumka-cli -- erase IN MASK OUT
cargo xtask bundle                                   # target/bundle/Gumka.app and a .dmg (unsigned)
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

`cargo` lives in `~/.cargo/bin`; add it to `PATH` if a shell doesn't find it. Linting for Linux
and Windows from a Mac needs a dummy Little CMS library; see
[docs/development.md](docs/development.md#checks).

## Before you finish a change

1. `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` and
   `cargo test --workspace` all pass. CI runs them on macOS, Windows and Linux with warnings as
   errors, on every pull request and before every nightly build and release.
2. Code that is cfg-gated or touches platform code also passes clippy for
   `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc`.
3. New behavior has tests, and a fixed bug has a test that fails without the fix.
4. Docs in `docs/` (and `README.md`, if users see the change) describe the new behavior.

## Rules that must not be broken

- **Untouched pixels stay bit-identical.** Pixels outside the erased region (the mask and its
  soft edge) must be identical to the input, in memory and in TIFF/PNG output. Never convert,
  re-quantize or round-trip them. Tests must keep proving it. A whole-image tool (Upscale)
  changes every pixel; its undo must bring back the original exactly.
- **Never lose the user's work or files.** Saves go through a temporary file, a sync and a
  rename. Every save keeps bit depth, gray or color, the ICC profile and the metadata the target
  format can hold. When something can't be kept correctly, refuse with a clear message instead of
  writing a wrong file. The pixels always come back from the worker thread, even on errors.
- **Files are untrusted input.** Parsers must never panic, hang, or allocate a size a file merely
  claims. Bounds-check every offset, use checked arithmetic, and drop a malformed metadata block
  rather than failing the open or the save. Fuzz new parsers.
- **Color is never changed silently.** Models get the stored values, converted only for linear
  or very wide profiles: inside the crop for Erase, per tile for Upscale, where the inspector
  says when colors beyond P3 may be clipped. The display converts a copy
  ([docs/color.md](docs/color.md)).
- **Models:** weights never go in git. The models the default tools need are bundled with the
  app; optional models may only ever be downloaded when the user asks, verified against
  `models.toml` (not built yet), never in the background. Every model is in `models.toml` with
  SHA-256, and its license must be GPL-compatible and allow commercial use
  ([docs/models.md](docs/models.md)).
- **Licensing:** every source file starts with `// SPDX-License-Identifier: GPL-3.0-or-later`;
  dependencies must be GPL-compatible (MIT, BSD, Apache-2.0).

## Code conventions

- Match the surrounding code: naming, idiom, error handling, and comment density. Comments are
  short and say why, not what.
- Make the smallest change that fixes the cause. No speculative abstractions; don't build later
  tools early.
- `unsafe` only in the OS and framework bindings: Core ML and Core Video in
  `gumka-infer/src/coreml.rs`, AppKit and QuartzCore in `gumka-app/src/display.rs`, `quit.rs`,
  `open_files.rs` and `delegate.rs`. Every `unsafe` block has a `// SAFETY:` comment; the
  workspace lints enforce it.
- Platform-specific code is `#[cfg(target_os = ...)]`-gated with a no-op stand-in for the other
  platforms, so every target builds.
- User-facing messages are short and specific ("<name> is read-only"), and say what the user can
  do instead.
- UI: neutral grays; the amber accent only for the primary action and the active tool. Colors
  and styles live in `gumka-app/src/theme.rs` ([docs/ui.md](docs/ui.md)).
- The app never names other products or trademarks (no "Capture One", "Lightroom"): it speaks of
  "your photo editor" or "your RAW editor".
- egui is 0.37: apps implement `App::ui` and `App::logic`; panels are
  `egui::Panel::top(..).show(ui, ..)`.

## Testing

- Unit tests live next to the code. Format round trips are in
  `crates/gumka-core/src/round_trips.rs`; app behavior is tested headless in `app.rs`'s tests
  with a fake worker; file panels go through the `panels` seam and must never open in a test.
- Tests must run without the downloaded models and without Core ML. A test that needs them is
  `#[ignore]`d. Use stand-ins (fake inpainters, segmenters, compile and load closures).
- Make test files (generated pictures, lcms2-built or macOS system profiles, hand-built bytes).
  Never use the user's personal photos.
- Driving the real app is sometimes needed: see
  [docs/development.md](docs/development.md#driving-the-real-app-macos). Launch, interact and
  capture in one shell call; stop the app with `kill -9`; run with a temporary `HOME`; don't
  share screenshots that show the user's files.
- **Core ML costs disk space.** Each model compile, and each new path a model is loaded from,
  adds a 170–215 MB plan to `~/Library/Caches/<process>/com.apple.e5rt.e5bundlecache` that is
  never removed. Avoid real Core ML runs in tests, reuse cache folders, and never delete or
  modify `~/Library/Caches/Gumka/models` or `models/` while testing.

## Working agreements

- Don't commit, push, tag or create branches unless asked. Pushing to `main` publishes a nightly
  build, and pushing a `release-*` tag publishes a release.
- Don't download models or add dependencies without a reason; pin versions, and prefer crates
  already in `Cargo.lock`. CI runner images, actions (by commit) and the Rust version are pinned
  too; change them only on purpose ([docs/packaging.md](docs/packaging.md#ci-and-releases)).
- Never send personal data (names, email addresses, files) to external services.
- Ask when a decision changes behavior users rely on (file formats, what is kept or dropped on
  save, defaults). Record decisions in the relevant document in `docs/`.

## Documentation map

| Document | Read it when you work on |
|---|---|
| [docs/overview.md](docs/overview.md) | Scope, status, roadmap |
| [docs/development.md](docs/development.md) | Setup, running, checks, tests, driving the app |
| [docs/architecture.md](docs/architecture.md) | Crates, modules, threads, platform code |
| [docs/opening-and-saving.md](docs/opening-and-saving.md) | Host apps, file arrival, quitting, saving |
| [docs/image-io.md](docs/image-io.md) | File formats and metadata |
| [docs/color.md](docs/color.md) | The preview's color management, the model's color space |
| [docs/erase-pipeline.md](docs/erase-pipeline.md) | The erase pipeline |
| [docs/inference.md](docs/inference.md) | Core ML, the compiled-model cache, later backends |
| [docs/models.md](docs/models.md) | The registry, model provenance, inputs and outputs |
| [docs/ui.md](docs/ui.md) | UI design and the logo |
| [docs/packaging.md](docs/packaging.md) | Packaging, distribution, licensing |
