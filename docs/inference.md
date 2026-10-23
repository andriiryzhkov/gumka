# Inference

Two ways to run a model, no more: locally (Core ML on macOS; ONNX Runtime planned for Windows and
Linux) or through an API (later; see [overview.md](overview.md#goals-and-scope)).

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

## Windows and Linux (later)

- ONNX Runtime via the `ort` crate, pinned to an EXACT version (2.0 is still a release
  candidate).
- Windows: Windows ML (needs Windows 11 24H2 for downloaded execution providers) or DirectML,
  with fallback. Linux: CUDA / ROCm / OpenVINO. CPU always as the last resort.
- Keep the ORT backend buildable on macOS as a reference: run the same crop through both
  backends and compare the outputs to catch conversion and precision problems.
- LaMa uses FFT layers: watch for unsupported-op CPU fallback in any non-native path.
