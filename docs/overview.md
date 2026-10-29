# Overview

Gumka is extensible AI photo retouching: erase, upscale, and more. A native Rust app with on-device
Core ML, optional cloud models later, working in 16 bits and wide gamut. It works standalone and
as the external editor of RAW editors; Capture One is the first one it is tested with.

License: GPL-3.0-or-later.

## Status (October 2026)

Working:

- LaMa through the Core ML backend in `gumka-infer` (output bit-identical to a Swift reference).
- The erase pipeline and TIFF / PNG / JPEG I/O with metadata in `gumka-core`.
- `gumka-cli erase`, `gumka-cli select` and `gumka-cli upscale`.
- The standalone app in `gumka-app` with the redesigned UI for the Erase tool: toolbar with
  cursor tools, tool rail, inspector with Selection / Model / Edges / Advanced / History, status
  bar, start screen; Before / After; erase on a worker thread; undo / redo; Save / Save As. Edges
  and Advanced drive `EraseSettings` (grow, feather, context).
- Select object with SAM 2.1 Small (Core ML): click to include, Option-click to exclude. The
  picture is encoded on the first click and again only after its pixels change. Each click that
  changes the mask is an undoable step, added to the mask from before the selection.
- Upscale, 1.25× to 4× (2× and 4× are the models' own output), with RealPLKSR (Core ML, our own
  conversion): in place and undoable, run in tiles on the worker thread with progress and
  Cancel ([upscale-pipeline.md](upscale-pipeline.md)).
- The Config page: Gumka's version, and the models, built from `models.toml` via
  `gumka_infer::models::registry()`.
- A color-managed preview ([color.md](color.md)).
- `cargo xtask bundle`: an unsigned `Gumka.app` and disk image with the models bundled, or on
  Linux an AppImage with ONNX Runtime and the models ([packaging.md](packaging.md)).

- Opening files sent by other apps on macOS: Finder's Open With, the Dock, a RAW editor's
  Edit With ([opening-and-saving.md](opening-and-saving.md#how-files-arrive)), and Save &
  Return to send the result back ([opening-and-saving.md](opening-and-saving.md#save--return)).

Not yet: other tools, the API backend, a native menu bar, recent files, signing and
notarization, downloads of optional models, `gumka-platform`, saving on a background thread.

Undecided: model choice per tool (options A/B/C on the design canvas).

The crate names and boundaries in [architecture.md](architecture.md) are a starting point; adjust
them if the code argues otherwise, but keep the principles.

## Goals and scope

- First tool: object removal (erase) with a brush mask and SAM click-to-select.
- Extensible tool model: each tool = input (mask / prompt / none) + model(s) + pipeline step.
  Upscaling is done. Planned later: outpainting / canvas extend, generative fill (undecided).
  Skipped: AI denoise and AI masking (Capture One already has them); depth-based background blur
  and relighting (dropped, October 2026).
- Two ways to run a model, no more: locally (Core ML on macOS, models bundled with the app;
  ONNX Runtime on Linux, the system's, on the CPU) or through an API (later).
  - API: bring-your-own key stored in the OS secret store; upload only the crop around the mask
    (a reduced copy for whole-image tools), never the full image; always opt-in with a clear
    indicator; asynchronous (task id + polling, progress, cancel).
  - A custom endpoint setting can point the API at a self-hosted server; no separate homelab
    backend.
  - Models too large to bundle (diffusion: Fill, Extend) are API-only.
- Ship the eraser first. Keep the tool seams clean, but don't build later tools early.

## Roadmap

1. Core ML spike (de-risk Rust ↔ Core ML): compile and cache the LaMa .mlpackage, load with
   CPU+GPU, print model input/output descriptions, run one inference on a test crop and mask,
   save the result, report timing. Repeat for SAM. (Done.)
2. `gumka-cli erase`: verify fill quality, color handling, bit-exactness outside the mask.
   (Done.)
3. egui app, standalone: open a TIFF (dialog or drag-and-drop), color-correct display, paint a
   mask, erase, undo, Save / Save As. (Done.)
4. "Edit With" round trip from a RAW editor on macOS, first Capture One. (Done: the app bundle,
   its document types, the open-documents handler and Save & Return.)
5. SAM click-to-select. (Done.)
6. Polish: brush-size shortcut (done), before/after toggle (done), model choice.
7. Upscale with RealPLKSR, 2× and 4×, then any factor from 1.25× to 4×. (Done.)
8. Linux: every tool on ONNX Runtime, on the CPU, from source and as an AppImage carrying
   ONNX Runtime and the models, built nightly and for releases. (Done.) Next: GPU execution
   providers, the display's color profile.

## Reference projects

- IOPaint (formerly lama-cleaner): LaMa + SAM web UI; repository archived August 2025. Useful for
  comparing results and as a pipeline reference.
- Scumble: open-source desktop AI inpainting editor on GitHub; a UX and design reference.

## Context

- Main platform: macOS (MacBook Air, Apple Silicon, wide-gamut P3 display). Also a Windows PC
  with a wide-gamut monitor. The photo catalog is on a NAS, so saving over SMB matters.
- The developer's background is C and JavaScript, with earlier ONNX Runtime and inpainting
  (LaMa crop-paste) work in darktable. This repository is independent of darktable (no shared
  code).
