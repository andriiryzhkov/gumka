# Architecture

## Stack

- Rust, eframe/egui; wgpu renders through Metal / DX12 / Vulkan.
- Cross-platform crates: `rfd` (native dialogs), `directories` (settings and cache paths).
  Planned: `keyring` for API keys (macOS Keychain / Windows Credential Manager / Linux Secret
  Service); optionally `muda` for a native macOS menu bar.
- `lcms2` (Little CMS, built statically) for color, `tiff`, `png` and `image` for files,
  `objc2` and its framework crates for Core ML and AppKit.

## Workspace

| Crate | Role |
|---|---|
| `gumka-core` | Pictures, file formats and metadata, color conversion, the erase and select pipelines. No UI, no ML. |
| `gumka-infer` | The model registry and the Core ML backend (`Inpainter` and `Segmenter` implementations). |
| `gumka-cli` | `gumka-cli erase` and `gumka-cli select`. |
| `gumka-app` | The egui app (binary `gumka`). |
| `xtask` | `cargo xtask models` downloads and checks the models in `models.toml`; `cargo xtask bundle` builds the macOS app ([packaging.md](packaging.md)). |

Proposed: `gumka-platform`, all OS-specific code behind traits in `macos.rs` / `windows.rs` /
`linux.rs`, selected with `#[cfg(target_os = ...)]`. Until it exists, platform code lives where
it is used, cfg-gated with no-op stand-ins elsewhere.

### gumka-core

- `lib.rs`: `Rgb16` (every picture in memory is 16-bit RGB), `Mask`, `Rect`, crop and
  `swap_region`.
- `picture.rs`: `Picture`: open, save (temporary file, sync, rename), orientation helpers.
- `tiff_io.rs`, `png_io.rs`, `jpeg_io.rs`: the formats; `meta.rs`: the one metadata record all
  of them read and write ([image-io.md](image-io.md)).
- `color.rs`: `DisplayTransform` for the preview and `ModelSpace` for the model
  ([color.md](color.md)).
- `erase.rs`: `ErasePlan` and `erase` ([erase-pipeline.md](erase-pipeline.md)).
- `select.rs`: the `Segmenter` trait, click scaling and `mask_from_scores`.

### gumka-infer

- `coreml.rs`: every `unsafe` Core ML and Core Video call; `load_cached` and the compiled-model
  cache; `MultiArray` and `PixelBuffer` ([inference.md](inference.md)).
- `lama.rs`: `CoreMlLama`, the `Inpainter`. `sam.rs`: `CoreMlSam`, the `Segmenter` (at most 14
  clicks per selection).
- `models.rs`: the built-in registry (`include_str!` of `models.toml`), the search folders
  (including the app bundle's compiled models) and `find_packages`.

### gumka-app

- `main.rs`: the window and the app icon (renders `assets/logo/gumka-icon.svg` with resvg).
- `app.rs`: `GumkaApp`: state, input, the toolbar, rail, inspector, status bar, start screen and
  dialogs, and the actions that wait for the worker.
- `canvas.rs`: the preview pyramid, view transform and brush painting. `document.rs`: the open
  document and its history (undo stores only changed regions).
- `worker.rs`: the inference thread. `models_page.rs`: the Models page.
- `display.rs`: tags the window's Metal layer Display P3 ([color.md](color.md)). `quit.rs`:
  routes macOS quits through "Save changes?"; `open_files.rs`: takes files other apps send
  ([opening-and-saving.md](opening-and-saving.md)). Both add methods to winit's app delegate
  through `delegate.rs`.
- `theme.rs`, `widgets.rs`, `icons.rs`: styling and shared widgets ([ui.md](ui.md)). Fonts are
  in `crates/gumka-app/assets/fonts` (IBM Plex, OFL).

## Threads

- The UI thread owns the document. Core ML models are not `Send`, so they are created on the
  inference thread (`worker.rs`) and stay there.
- The picture's pixels travel to the inference thread when a model needs them (an erase, or
  encoding the picture for selection) and come back with the result. They always come back: a
  panic while handling a request becomes an error, a model that panics while loading is only
  unavailable, and a request the thread can no longer take is answered without it.
- While the pixels are away, actions that need them wait instead of being refused: a waiting
  save has its own slot and runs first, then the waiting request (open, open panel, quit), once
  the pixels are back (`run_waiting`, called from `App::logic`). An erase in flight counts as an
  unsaved change.
- Selections carry an id; replies for an older selection or another document are ignored, and
  document generations come from one counter shared by all documents.

## Canvas

- The mask is a separate texture with partial updates while painting.
- A pyramid of textures (full size, 1/2, 1/4, ...) stands in for mipmaps, so zoomed-out views
  don't shimmer. Its size comes from the GPU's texture limit, which egui only knows from the
  first frame, so a file given at startup is opened on the first frame.
- A drag's role (pan, paint or neither) is decided when it starts; zoom, scroll, Return, Esc and
  undo are ignored while a stroke is painted.
- Possible later: egui `Scene` for pan and zoom; a custom wgpu paint callback if GPU blending is
  needed; tiles for pictures beyond the GPU texture limit.

## unsafe

- The workspace denies `unsafe_op_in_unsafe_fn` and warns on `undocumented_unsafe_blocks`
  (an error in CI): every `unsafe` block has a `// SAFETY:` comment.
- `unsafe` stays in the OS and framework bindings: Core ML and Core Video in
  `gumka-infer/src/coreml.rs`, AppKit and QuartzCore in `gumka-app/src/display.rs`, `quit.rs`,
  `open_files.rs` and `delegate.rs`.
