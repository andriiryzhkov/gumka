# Inference

Two ways to run a model, no more: locally (Core ML on macOS, ONNX Runtime on Linux; Windows
later) or through an API (later; see [overview.md](overview.md#goals-and-scope)).

## Core ML (macOS)

- Native Core ML via `objc2`, `objc2-foundation`, `objc2-core-ml` and `objc2-core-video`. The
  current LaMa takes and returns pixel buffers; SAM's encoder outputs go to its decoder as Core
  ML arrays without copying (`coreml::MultiArray`). All `unsafe` Core ML and Core Video calls
  stay in `gumka-infer/src/coreml.rs`.
- RealPLKSR takes and returns float32 arrays, so 16-bit pictures keep their precision; it
  computes in float16. `MultiArray` copies a row at a time, since Core ML pads only between
  rows; a 512 px tile goes in and out in milliseconds against a run of over a second.
- Choose compute units per model: start LaMa on CPU+GPU; enable the Neural Engine only after its
  output matches the reference (conversions can change numerics on different units). RealPLKSR
  stays on CPU+GPU: on the Neural Engine its output is wrong (29-36 dB against PyTorch) and
  hardly faster.
- Measured on the MacBook Air, LaMa 800×800: about 0.7 s per run on CPU+GPU, about 0.75 s with
  All (the Neural Engine adds nothing), about 2 s on the CPU only. Outputs agree within 1/255
  across units.

## The compiled-model cache

`coreml::load_cached` compiles a `.mlpackage` to `.mlmodelc` once, at first load (about 0.3 s
for LaMa), and caches it in `~/Library/Caches/Gumka/models`:

- An entry, `<stem>-<fingerprint>.mlmodelc`, counts only with its `.gumka-complete` file. A
  compile is copied in under a temporary name, marked, and renamed into place in one step, so
  readers only ever see complete entries; when two processes compile at once, the first rename
  wins.
- An already compiled `.mlmodelc` (what the app bundle carries) is copied into the cache in place
  of a compile (a clone on APFS), so it is loaded from the same path wherever the app is.
- An entry that doesn't load is compiled once more.
- Entries are removed only after being renamed aside. One that turns out to be another process's
  fresh entry (complete, and not the inode that didn't load) is put back, so parallel runs don't
  delete each other's.
- Entries unused for 30 days are removed (the marker's time is the last use), so the CLI's and
  the app's models folders don't evict each other. Temporary leftovers go after an hour.
- When the cache folder can't take an entry, it goes into `$TMPDIR/gumka-models` (the same
  protocol and cleanup), used only if that folder is the user's alone (without `$TMPDIR` it would
  be in the shared `/tmp`). Once the fallback holds an entry, Gumka keeps loading from it even
  after the cache folder is writable again (accepted: it keeps one path).
- Only if neither can be written is the model loaded from Core ML's own output, removed with the
  model (not at quit: the worker thread's models aren't dropped then; macOS cleans `$TMPDIR`).
- Existing entries without a marker (from older builds) are compiled once more.
- Tests use stand-ins for compiling and loading, not Core ML.

## Core ML's plan cache

Core ML keeps compiled plans of its own in
`~/Library/Caches/<process name>/com.apple.e5rt.e5bundlecache` (`gumka`, `gumka-cli`), about
170–215 MB each (CPU plans as big as GPU ones), and never removes them; Gumka's cleanup can't
reach them. It adds one for every compile, even into the same path, and one for every new path a
model is loaded from. So every recompile (after an upgrade, a lost marker, a broken entry) leaves
one behind, and models are always loaded from paths that stay the same. See
[development.md](development.md#core-mls-plan-cache-costs-disk-space) for what this means for
testing.

## ONNX Runtime (Linux)

- The `ort` crate, pinned to an exact version (`=2.0.0-rc.15`; 2.0 is still a release
  candidate), with `load-dynamic`: Gumka doesn't link ONNX Runtime but opens it when the first
  model loads, the AppImage's own (Microsoft's 1.31.0 build) or the system's. A build needs no ONNX Runtime (CI has none), and
  without one the models are only unavailable, with a message saying what to install and to
  restart Gumka (the failure is kept: the app loads its models once). A library that is there
  but doesn't open (a library it needs is missing, say) gives dlopen's reason instead. The
  upscalers, which load when first used, are checked against ONNX Runtime at startup too, and
  the Models page's card says when it is missing. `unsafe` stays in `ort`;
  `gumka-infer/src/onnx` has none.
- Built for API 17, so ONNX Runtime 1.17 or later. The library is looked for as
  `$ORT_DYLIB_PATH` alone, if set; else the package's (`<prefix>/lib/libonnxruntime.so.1` for
  `<prefix>/bin/gumka`: the AppImage's, the one it was tested with); `libonnxruntime.so.1`
  (ONNX Runtime's own builds),
  `libonnxruntime.so.1.<minor>` (Debian's and Ubuntu's, newest first) and `libonnxruntime.so`
  (the development link), each in the executable's folder first, then the system's library
  path.
- Telemetry is off: Microsoft's builds record and send usage data unless told not to, and
  `ort` turns it on. The environment is created with it off, and the AppImage sets
  `ORT_DISABLE_TELEMETRY=1` before ONNX Runtime starts, without which it still keeps a device
  id on disk ([packaging.md](packaging.md#telemetry)).
- Models run on the CPU (`ONNX Runtime · CPU`), with ONNX Runtime's default thread pool and
  graph optimizations. All three are fixed-size ONNX exports of the same models as on macOS
  ([models.md](models.md)); the adapters in `onnx/lama.rs`, `sam.rs` and `upscale.rs` check the
  inputs' names and shapes when loading and convert pixels to and from float tensors.
- ORT sessions are created on the inference thread and stay there, like Core ML models.
- Ubuntu's `libonnxruntime1.23` prints "Schema error: Trying to register schema ..." for every
  ONNX operator when it loads (it links the system `libonnx` beside its own copy of the
  schemas). It is harmless: the models load and run.
- Measured on a Ryzen 7 8845HS (8 cores, 16 threads), CPU: SAM encodes a picture in about
  2.5 s and selects in 0.1 s; LaMa erases in about 1.6 s; RealPLKSR 4× takes about 1.8 s per
  256 px tile.
- Checked: the SAM export's masks match darktable's own SAM 2.1 export exactly on generated
  pictures; `cargo test -p gumka-infer -- --ignored` runs all three on real ONNX Runtime.

### Later

- GPU execution providers: CUDA / ROCm / OpenVINO on Linux; on Windows, Windows ML (needs
  Windows 11 24H2 for downloaded execution providers) or DirectML, with fallback. CPU always as
  the last resort. The ONNX backend is gated to Linux; Windows needs only its registry entries
  and the gate widened.
- Keep the ORT backend buildable on macOS as a reference: run the same crop through both
  backends and compare the outputs to catch conversion and precision problems.
- LaMa uses FFT layers: watch for unsupported-op CPU fallback in any non-CPU provider.
- candle (pure Rust) was considered in place of ONNX Runtime and set aside (October 2026): each
  model would need its architecture written in Rust (it has SAM 1, not SAM 2, no LaMa or
  RealPLKSR), it has no FFT op for LaMa, and its GPUs are CUDA and Metal only. macOS keeps
  Core ML.
