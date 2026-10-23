# Architecture

## Stack

- Rust, eframe/egui; wgpu renders through Metal / DX12 / Vulkan.
- Cross-platform crates: `rfd` (native dialogs), `directories` (settings and cache paths).
  Planned: `keyring` for API keys (macOS Keychain / Windows Credential Manager / Linux Secret
  Service); optionally `muda` for a native macOS menu bar.
- `lcms2` (Little CMS, built statically) for color, `tiff`, `png` and `image` for files,
  `fast_image_resize` for Upscale's resampling, `objc2` and its framework crates for Core ML and
  AppKit, `ort` (loading the system's ONNX Runtime) on Linux.

## Workspace

| Crate | Role |
|---|---|
| `gumka-core` | Pictures, file formats and metadata, color conversion, the erase, select and upscale pipelines. No UI, no ML. |
| `gumka-infer` | The model registry and the backends: Core ML on macOS, ONNX Runtime on Linux (`Inpainter`, `Segmenter` and `Upscaler` implementations). |
| `gumka-cli` | `gumka-cli erase`, `select` and `upscale`. |
| `gumka-app` | The egui app (binary `gumka`). |
| `xtask` | `cargo xtask models` downloads and checks the models in `models.toml`; `cargo xtask bundle` builds the macOS app (`bundle.rs`) or the Linux AppImage (`appimage.rs`), with what both need in `package.rs` ([packaging.md](packaging.md)). |

Proposed: `gumka-platform`, all OS-specific code behind traits in `macos.rs` / `windows.rs` /
`linux.rs`, selected with `#[cfg(target_os = ...)]`. Until it exists, platform code lives where
it is used, cfg-gated with no-op stand-ins elsewhere.

### gumka-core

- `lib.rs`: `Rgb16` (every picture in memory is 16-bit RGB), `Mask`, `Rect`, crop,
  `swap_region`, and `by_rows`, which splits work over the cores by rows when it is large.
- `picture.rs`: `Picture`: open, save (temporary file, sync, rename), `swap_pixels` (pixels
  of another size, with an alpha to match), orientation helpers.
- `tiff_io.rs`, `png_io.rs`, `jpeg_io.rs`: the formats; `meta.rs`: the one metadata record all
  of them read and write ([image-io.md](image-io.md)).
- `color.rs`: `DisplayTransform` for the preview and `ModelSpace` for the model
  ([color.md](color.md)).
- `erase.rs`: `ErasePlan` and `erase` ([erase-pipeline.md](erase-pipeline.md)).
- `select.rs`: the `Segmenter` trait, click scaling and `mask_from_scores`.
- `upscale.rs`: the `Upscaler` trait, `Factor`, `result_size` and the tiled `upscale`
  ([upscale-pipeline.md](upscale-pipeline.md)).

### gumka-infer

- `coreml.rs`: every `unsafe` Core ML and Core Video call; `load_cached` and the compiled-model
  cache; `MultiArray` and `PixelBuffer` ([inference.md](inference.md)).
- `lama.rs`: `CoreMlLama`, the `Inpainter`. `sam.rs`: `CoreMlSam`, the `Segmenter` (at most 14
  clicks per selection). `upscale.rs`: `CoreMlUpscaler`, the `Upscaler` (RealPLKSR 2× and 4×).
- `onnx/` (Linux): `mod.rs` loads the system's ONNX Runtime and opens sessions; `lama.rs`,
  `sam.rs` and `upscale.rs` hold `OnnxLama`, `OnnxSam` and `OnnxUpscaler`
  ([inference.md](inference.md#onnx-runtime-linux)).
- `models.rs`: the built-in registry (`include_str!` of `models.toml`), this platform's entry
  of each model, the search folders (including the app bundle's compiled models) and
  `find_packages`.

### gumka-app

- `main.rs`: the window and the app icon (renders `assets/logo/gumka-icon.svg` with resvg).
- `app.rs`: `GumkaApp`: state, input, the toolbar, rail, inspector, status bar, start screen and
  dialogs, and the actions that wait for the worker.
- `canvas.rs`: the preview pyramid, view transform and brush painting. `document.rs`: the open
  document and its history (undo stores only changed regions, or the whole picture for a
  change of size).
- `worker.rs`: the inference thread. `models_page.rs`: the Models page.
- `display.rs`: tags the window's Metal layer Display P3 ([color.md](color.md)). `quit.rs`:
  routes macOS quits through "Save changes?"; `open_files.rs`: takes files other apps send
  ([opening-and-saving.md](opening-and-saving.md)). Both add methods to winit's app delegate
  through `delegate.rs`.
- `theme.rs`, `widgets.rs`, `icons.rs`: styling and shared widgets ([ui.md](ui.md)). Fonts are
  in `crates/gumka-app/assets/fonts` (IBM Plex, OFL).

## Threads

- The UI thread owns the document. Core ML models are not `Send`, so they are created on the
  inference thread (`worker.rs`) and stay there; ONNX Runtime sessions too.
- The picture's pixels travel to the inference thread when a model needs them (an erase, an
  upscale, or encoding the picture for selection) and come back with the result. They always
  come back: a panic while handling a request becomes an error, a model that panics while
  loading is only unavailable, and a request the thread can no longer take is answered without
  it.
- While the pixels are away, actions that need them wait instead of being refused: a waiting
  save has its own slot and runs first, then the waiting request (open, open panel, quit), once
  the pixels are back (`run_waiting`, called from `App::logic`). An erase in flight counts as an
  unsaved change.
- Selections carry an id; replies for an older selection or another document are ignored, and
  document generations come from one counter shared by all documents.
- An upscale reports progress after each tile and checks a shared cancel flag; the upscalers
  load on first use, so startup only checks that their files are there.

## Canvas

- The picture and the mask overlay are each a pyramid of textures (full size, 1/2, 1/4, ...,
  down to about 256 px), which stands in for mipmaps, so zoomed-out views don't shimmer. Every
  level is cut into tiles of 2046 texels (less if the GPU's texture limit is lower), so a
  picture beyond the limit still shows every pixel at 100%. Each tile's texture also holds a
  texel of its neighbours on every side (2048 in all), so that filtering across a tile's edge
  reads what one texture of the whole level would; the quads leave that apron out. Each tile
  is one quad, turned by the picture's orientation, and only tiles on screen are drawn.
- Only the coarsest level is made up front, when a picture opens and after an upscale or its
  undo, so neither waits on the whole picture (a 256 MP upscale's coarsest levels take about
  0.1 s). A tile is made when it first comes into view, from the pixels as they are then. Each
  frame makes tiles until 25 ms have passed, but always at least one, so a frame that makes a
  tile averaging much of a very large picture (a coarse level of it) takes longer, about as
  long as that tile. Until a tile is made, the same part of the next coarser level that is
  made stands in, down to the coarsest. None are made while the pixels are with the worker.
  Edits redraw only the tiles made so far, aprons included; the mask's while painting too.
  Rendering is split over the cores by rows when it reads many pixels, and stays on the UI
  thread for small regions such as a brush dab (`gumka_core::by_rows`).
- Before / After is one pyramid made in full when the picture opens, from the finest level a
  texture of the GPU's limit holds. egui only knows that limit from the first frame, so a file
  given at startup is opened on the first frame. After an upscale it keeps the opened size and
  is drawn over the same rectangle.
- After an upscale (or its undo) the picture and mask pyramids are rebuilt at the new size, the
  zoom divided so the photo keeps its place on screen.
- A drag's role (pan, paint or neither) is decided when it starts; zoom, scroll, Return, Esc and
  undo are ignored while a stroke is painted.
- Possible later: egui `Scene` for pan and zoom; a custom wgpu paint callback if GPU blending is
  needed; dropping tiles long out of view, as the tiles made at 100% of a very large picture
  can take a lot of GPU memory.

## unsafe

- The workspace denies `unsafe_op_in_unsafe_fn` and warns on `undocumented_unsafe_blocks`
  (an error in CI): every `unsafe` block has a `// SAFETY:` comment.
- `unsafe` stays in the OS and framework bindings: Core ML and Core Video in
  `gumka-infer/src/coreml.rs`, AppKit and QuartzCore in `gumka-app/src/display.rs`, `quit.rs`,
  `open_files.rs` and `delegate.rs`.
