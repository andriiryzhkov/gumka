# Development

## Requirements

- Rust, installed with [rustup](https://rustup.rs). The version is pinned in
  `rust-toolchain.toml` (rustup installs it on first use, with clippy and rustfmt), the same
  locally and in CI. Raise it on purpose, in its own change: new releases bring new lints, and CI
  treats warnings as errors.
- macOS 13 or later to run the models (Core ML's asynchronous compile needs it). Apple Silicon is
  the main target. `MACOSX_DEPLOYMENT_TARGET = "13.0"` is set in `.cargo/config.toml` (a value
  already in the environment wins); the app bundle's Info.plist must say the same
  (`LSMinimumSystemVersion` 13.0).
- Xcode is not needed: Core ML compiles models at runtime.
- Linux: the models run on ONNX Runtime 1.17 or later, the system's, opened when the first
  model loads (`libonnxruntime`; on Ubuntu 26.04, `sudo apt install libonnxruntime1.23`). Set
  `ORT_DYLIB_PATH` to use another build. Building needs no ONNX Runtime
  ([inference.md](inference.md#onnx-runtime-linux)).
- Windows builds and must keep building, but has no model backend yet.

## Models

```sh
cargo xtask models           # download what is missing into models/ and check every file
cargo xtask models --check   # only check
```

`cargo xtask models` reads the registry, `models.toml`, and downloads into `models/`
(git-ignored), checking each file's size and SHA-256. See [models.md](models.md) for each
model's provenance. Adding a model is an entry in `models.toml`, plus code only if it needs a new
adapter.

Only this platform's models are downloaded: Core ML on macOS (about 340 MB), ONNX on Linux
(about 450 MB, each model in a folder of its own, such as `models/LaMa-ONNX`).

Models are looked up, in order, in `$GUMKA_MODELS`, the app bundle's
`Contents/Resources/models` (when running from one; the packages there are compiled),
`./models`, the repository's `models/`, then `~/Library/Application Support/Gumka/models`
(`~/.local/share/gumka/models` on Linux) (`gumka-infer/src/models.rs`):

- A folder counts only if it holds every file `models.toml` lists, each at the listed size (no
  SHA-256 at startup; `cargo xtask models --check` is the full check). Incomplete copies are
  skipped with a log line and named in the error, which also lists the folders searched and the
  command to fetch the models.
- `$GUMKA_MODELS` is the override: its files must exist but may differ in size, with a warning.

Compiled Core ML models are cached in `~/Library/Caches/Gumka/models`
([inference.md](inference.md)).

### Converting models

A model we convert ourselves has its script in `tools/convert/<model>/`. The scripts are Python,
with dependencies pinned in `pyproject.toml` and `uv.lock`, and are run with
[uv](https://docs.astral.sh/uv/). They aren't part of the build: run one by hand when its model
changes, publish the output, then pin it in `models.toml`.

- `tools/convert/realplksr/` converts the Upscale models. Its README covers running it, what it
  checks, and publishing the result.
- Each script downloads only pinned, checksummed sources, and checks its output against the
  original framework on generated inputs, never on personal photos.
- The `.venv/` and `out/` folders are git-ignored. Weights never go in git.

## Running

```sh
cargo run --release -p gumka-app [-- IMAGE]
cargo run --release -p gumka-cli -- erase IN MASK OUT [--units cpu|gpu|ane|all]
cargo run --release -p gumka-cli -- select IN OUT_MASK --click X,Y [--exclude X,Y]
cargo run --release -p gumka-cli -- upscale IN OUT --scale FACTOR [--units cpu|gpu]
cargo run --release -p gumka-infer --example coreml_spike -- models/LaMa.mlpackage \
    [--units cpu|gpu|ane|all] [IMAGE.png MASK.png OUT.png [REFERENCE.png]]
```

- CLI masks cover IN's pixels as stored, before EXIF orientation is applied, as `select` saves
  them. Opaque pixels that aren't black are erased; transparent and black ones are kept. The CLI
  warns when IN has an orientation and when MASK is a JPEG (compression noise near black counts
  as erase), and a mask the size of the turned picture is refused with an error naming the
  orientation.
- `upscale` refuses `--units ane` and `all` (RealPLKSR gives wrong results on the Neural
  Engine), and a scale whose result would be too large: it says to use a smaller one or, when
  not even 1.25× fits, that the picture is already as large as upscale can make it. An unknown `--units`
  value gets an error that names it.
- On Windows and Linux the app takes the file as a command-line argument; on macOS it will come
  as an "open documents" event ([opening-and-saving.md](opening-and-saving.md)).
- `cargo xtask bundle` builds the macOS app and disk image in `target/bundle`
  ([packaging.md](packaging.md)); `open target/bundle/Gumka.app` runs it.

## Checks

Run all of these before a change is done. CI runs the first three on macOS, Windows and Linux
with `RUSTFLAGS=-D warnings` on every pull request, and before every nightly build and release
(`.github/workflows`; see [packaging.md](packaging.md#ci-and-releases)).

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Linux and Windows can be linted from a Mac without their C toolchains. Little CMS (`lcms2-sys`)
is C code built with `cc`, which can't cross-compile here, but clippy doesn't link, so a dummy
library lets the build script skip it:

```sh
rustup target add x86_64-unknown-linux-gnu x86_64-pc-windows-msvc   # once
mkdir -p /tmp/fake-lcms && : > /tmp/fake-lcms/liblcms2.a
LCMS2_LIB_DIR=/tmp/fake-lcms cargo clippy --workspace --all-targets \
    --target x86_64-unknown-linux-gnu -- -D warnings
LCMS2_LIB_DIR=/tmp/fake-lcms cargo clippy --workspace --all-targets \
    --target x86_64-pc-windows-msvc -- -D warnings
```

## Tests

- Unit tests live next to the code (`#[cfg(test)] mod tests`). Larger suites:
  - `gumka-core/src/round_trips.rs`: every source → target format pair, with metadata, thumbnail,
    gray, alpha, resolution, malformed-input and allocation checks (a test-only global allocator
    proves claimed sizes are never allocated).
  - `gumka-core/tests/model_space.rs`: the color conversion for the model (linear, very wide and
    camera profiles).
  - `gumka-app/src/app.rs` tests: a headless harness that runs the app's `logic` and `ui` with a
    fake worker (`eframe::Frame::_new_kittest()`); file panels go through a `panels` seam that
    only counts calls under `cfg(test)`, so no test can open a real panel.
- Tests must not need the downloaded models, Core ML or ONNX Runtime. A test that does is
  `#[ignore]` and runs with `cargo test -p gumka-infer -- --ignored` (on Linux, against the
  system's ONNX Runtime; `--release` makes it several times faster). The compiled-model cache protocol is tested
  with stand-in compile and load closures.
- What must stay covered everywhere: pixels outside the mask bit-identical; color and ICC round
  trips; metadata preservation across formats; parsers that never panic, hang or over-allocate
  on malformed input.
- Test files are generated (sips, lcms2-built profiles, hand-built bytes) or made from macOS
  system profiles. Never use personal photos.

## Driving the real app (macOS)

For checks that need the GUI (dialogs, quitting, drag and drop, the screen's colors):

- Build with `cargo build --release -p gumka-app`; the binary is `target/release/gumka`.
- A process started from a shell tool dies when that shell call ends, so launch, interact and
  capture in one call: `./target/release/gumka FILE & PID=$!; sleep 3; …; kill -9 $PID`. Never
  `wait` on it; a plain `kill` may not stop it.
- Find the window with `CGWindowListCopyWindowInfo` (owner name `gumka`, layer 0) and capture
  it with `screencapture -x -o -l <window id> out.png`. Screenshots are in the display's color
  space with its profile embedded; convert through that profile before comparing colors.
- Post mouse and key events with `CGEvent` (Swift scripts work well). Synthetic clicks need
  about 250 ms after moving the pointer and about 120 ms between press and release.
- Run with a temporary `HOME` so the user's caches, settings and the open panel's Recents stay
  out of the test. Don't share screenshots that show the user's files.

## Core ML's plan cache costs disk space

Core ML keeps compiled plans of its own in
`~/Library/Caches/<process name>/com.apple.e5rt.e5bundlecache` (`gumka`, `gumka-cli`), about
170–215 MB each, and never removes them ([inference.md](inference.md)). It adds one for every
compile and for every new path a model is loaded from. Test runs that compile models into many
fresh cache folders fill the disk quickly (one session added 16 GB). So:

- prefer stand-ins over real Core ML loads in tests;
- reuse one or two cache folders (a shared temporary `HOME`) across runs;
- never delete or modify `~/Library/Caches/Gumka/models` or `models/` as part of a test; work
  on copies.

## egui notes

- egui 0.37: apps implement `App::ui(&mut self, ui, frame)` (plus `App::logic`, which eframe
  also runs while the window is hidden or minimized); panels are
  `egui::Panel::top(..).show(ui, ..)`.
- The wgpu renderer has no mipmaps, hence the preview texture pyramid in
  `gumka-app/src/canvas.rs`.
