# Models

## The registry

Gumka's models are listed in [`models.toml`](../models.toml): each model's source, license and
files, with pinned download URLs, sizes and SHA-256 checksums. Weights never go in git.

```sh
cargo xtask models           # download what is missing into models/ and check every file
cargo xtask models --check   # only check
```

The app builds the registry in (`gumka_infer::models::registry()`) for the Models page and to
find packages ([development.md](development.md#models) has the search folders). Adding a model is
an entry in `models.toml`, plus code only if it needs a new adapter.

## Rules

- Every model must have a license compatible with GPL-3.0-or-later that allows commercial use.
  Never include non-commercial-only models. Each model's license text ships in the app bundle.
- The models the default tools need (LaMa, SAM) are bundled with the app: `cargo xtask bundle`
  compiles them to `.mlmodelc` from checked downloads ([packaging.md](packaging.md)). The app
  works offline from its first launch, and its contents are what was tested.
- Optional and large models (a second erase model, upscalers; planned): downloaded only when the
  user asks, from the Models page, into `~/Library/Application Support/Gumka/models`, verified
  against `models.toml` (size and SHA-256), never in the background; they survive app updates.
  Keep the bundle under about 500 MB. Multi-gigabyte generative models (Fill, Extend) use the
  API, or an optional download with a clear disk-space warning.
- A user models folder must hold complete copies of the registry's files; a different model in
  place of one is given through `$GUMKA_MODELS`.
- Keep this file listing each model, its source and license, and what goes in and out of it.

| Model | Used for | Format | Source | License |
|---|---|---|---|---|
| LaMa (Big LaMa) | Erase | Core ML `.mlpackage` | [mlboydaisuke/LaMa-CoreML](https://huggingface.co/mlboydaisuke/LaMa-CoreML) | Apache-2.0 |
| SAM 2.1 Small | Select object | Core ML, 3 × `.mlpackage` | [apple/coreml-sam2.1-small](https://huggingface.co/apple/coreml-sam2.1-small) | Apache-2.0 |

LaMa and SAM 2 are Apache 2.0, compatible with GPL-3.0.

## LaMa (Core ML)

- Upstream: [advimman/lama](https://github.com/advimman/lama), "Resolution-robust Large Mask
  Inpainting with Fourier Convolutions" (Suvorov et al., 2021). Apache-2.0.
- Core ML conversion: Daisuke Majima (john-rocky), from
  [CoreML-Models](https://github.com/john-rocky/CoreML-Models). Apache-2.0.
- Inputs: `image` 800×800 BGRA 8-bit; `mask` 800×800 one-channel 8-bit (L008, non-zero = fill).
  Output: `output` 800×800 BGRA 8-bit. Weights are FP32.
- The 8-bit I/O means the fill is 8-bit before it is blended into the 16-bit image. If that shows
  banding in smooth areas, make our own conversion with float tensor I/O via the CoreMLaMa script
  ([mallman/CoreMLaMa](https://github.com/mallman/CoreMLaMa); PyTorch → coremltools;
  coremltools no longer converts from ONNX).

## SAM 2.1 Small (Core ML)

- Upstream: [facebookresearch/sam2](https://github.com/facebookresearch/sam2), "SAM 2: Segment
  Anything in Images and Videos" (Ravi et al., 2024). Apache-2.0.
- Core ML conversion: Apple, float16, from Hugging Face's
  [conversion fork](https://github.com/huggingface/segment-anything-2/tree/coreml-conversion).
  Apache-2.0. [SAM2 Studio](https://github.com/huggingface/sam2-studio) is the reference app
  and the wiring reference. Several clicks describe one object, as in SAM.
- Three models:
  - Image encoder: `image` 1024 × 1024 BGRA (the picture stretched) → `image_embedding`
    [1, 256, 64, 64], `feats_s0` [1, 32, 256, 256], `feats_s1` [1, 64, 128, 128].
  - Prompt encoder: `points` [1, N, 2] in 1024 × 1024 coordinates, `labels` [1, N] (1 include,
    0 exclude) → `sparse_embeddings`, `dense_embeddings`.
  - Mask decoder: the five arrays above → `low_res_masks` [1, 3, 256, 256] (scores; above 0 is
    selected) and `scores` [1, 3]; Gumka keeps the best of the three. `sparse_embedding` is
    [1, 2..15, 256]: the clicks plus a padding point, so one selection takes at most 14 clicks.
- The encoder outputs go to the decoder as Core ML arrays without copying
  (`coreml::MultiArray`).

## Candidates and exclusions

- Later, a fast second erase model: MI-GAN (Picsart, MIT code and weights, 512 × 512, about
  14 MB). Convert it ourselves from the official PyTorch export and check it against their ONNX
  pipeline; the third-party Core ML conversions (`ewchampion/migan-coreml-*`) have no model card
  and give a grid artifact over the whole output.
- MAT is CC BY-NC (research only): not usable.
- ONNX versions of LaMa and SAM for the ORT backend ([inference.md](inference.md)).
