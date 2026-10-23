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

Each entry names its `platform`: `macos` for Core ML, `linux` for ONNX. An id has at most one
entry per platform; the app asks for this platform's (`models::model`), and the Models page and
`cargo xtask models` list only this platform's (`--all` downloads every one). An entry with a
`dir` keeps its files in that folder of a models directory (the ONNX models, whose file names
are generic); its `packages` and file paths are relative to it. An ONNX package's weights may sit
beside it as `<package>_data`.

## Rules

- Every model must have a license compatible with GPL-3.0-or-later that allows commercial use.
  Never include non-commercial-only models. Each model's license text ships in the app bundle.
- The models the tools need (LaMa, SAM, the RealPLKSR upscalers) are bundled with the app:
  `cargo xtask bundle` compiles them to `.mlmodelc` from checked downloads for the macOS app,
  and puts the ONNX files in the AppImage as downloaded ([packaging.md](packaging.md)). The app works offline from its first launch, and its contents
  are what was tested. Small models are bundled; the upscalers are 15 MB each.
- Optional and large models (a second erase model; planned): downloaded only when the user asks, from the Models page, into `~/Library/Application Support/Gumka/models`, verified
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
| RealPLKSR 2× and 4× | Upscale | Core ML `.mlpackage` each, our conversion | [gumka/realplksr-coreml](https://huggingface.co/gumka/realplksr-coreml), from dslisleedh/PLKSR (weights by neosr-project) | MIT |
| LaMa (Big LaMa), Linux | Erase | ONNX | [Carve/LaMa-ONNX](https://huggingface.co/Carve/LaMa-ONNX) | Apache-2.0 |
| SAM 2.1 Small, Linux | Select object | ONNX, 2 models with external weights | [onnx-community/sam2.1-hiera-small-ONNX](https://huggingface.co/onnx-community/sam2.1-hiera-small-ONNX) | Apache-2.0 (upstream's; the repository states none) |
| RealPLKSR 2× and 4×, Linux | Upscale | ONNX | [darktable-org/upscale-realplksr-onnx](https://huggingface.co/darktable-org/upscale-realplksr-onnx) | MIT |

LaMa and SAM 2 are Apache 2.0, RealPLKSR is MIT; all are compatible with GPL-3.0. MIT names its
copyright holder, so the registry's `copyright` lines go into the bundled notice before the text.

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

## RealPLKSR (Core ML)

- Upstream: PLKSR, "Partial Large Kernel CNNs for Efficient Super-Resolution" (Dongheon Lee et
  al., 2024), <https://github.com/dslisleedh/PLKSR>, MIT, "Copyright (c) 2024 Dongheon Lee".
  RealPLKSR, its variant for real-world photos, and the checkpoints are neosr-project's,
  released under the same MIT license in
  [PLKSR issue #4](https://github.com/dslisleedh/PLKSR/issues/4).
- The "MSSIM pretrain" checkpoints, 2× and 4× (7.4 M parameters, pixel-shuffle upsampler),
  trained without GAN fine-tuning: they sharpen what is there rather than invent detail. Taken
  from darktable's mirror, `darktable-org/upscale-realplksr-onnx`, at a pinned revision and
  checked by SHA-256.
- Core ML conversion: ours, `tools/convert/realplksr/convert.py` (spandrel, `torch.jit.trace`,
  coremltools 9.0; ML Program, float16 compute, macOS 13). Published at
  [gumka/realplksr-coreml](https://huggingface.co/gumka/realplksr-coreml) and pinned in
  `models.toml`.
- Inputs: `input` float32 [1, 3, 512, 512], planar RGB from 0 to 1. Output: `output` float32
  [1, 3, 1024, 1024] (2×) or [1, 3, 2048, 2048] (4×). The pipeline tiles pictures with 16 px of
  context per side ([upscale-pipeline.md](upscale-pipeline.md)).
- Checked against PyTorch on generated tiles: 72-75 dB PSNR on CPU and GPU, with no seams
  (errors at tile borders no larger than elsewhere). On the Neural Engine it is wrong (29-36 dB)
  and hardly faster, so it runs on CPU and GPU.
- About 1.3 s (2×) and 1.5 s (4×) per tile on an M4: about 15 MFLOP per input pixel, so a 24 MP
  photo takes 2 to 3 minutes.

## LaMa (ONNX, Linux)

- Carve's export of Big LaMa (`lama_fp32.onnx`), Apache-2.0, with the Fourier convolutions
  rewritten so that it exports; darktable uses the same file.
- Inputs: `image` float32 [1, 3, 512, 512], RGB from 0 to 1; `mask` float32 [1, 1, 512, 512],
  1 fills. Output: `output` float32 [1, 3, 512, 512] from 0 to 255, already joined to the input
  outside the mask. The adapter rounds it to 8 bits, like Core ML's output, before the pipeline
  blends it in.

## SAM 2.1 Small (ONNX, Linux)

- onnx-community's export (Transformers.js layout), fp32, pinned by revision. The repository
  states no license and has no model card; the weights are Meta's SAM 2.1 Hiera Small,
  Apache-2.0, as in `facebook/sam2.1-hiera-small`.
- Two models, each with its weights in an `.onnx_data` file beside it:
  - `vision_encoder.onnx`: `pixel_values` [1, 3, 1024, 1024] (the picture stretched, RGB from 0
    to 1, normalized with ImageNet's mean and deviation) → `image_embeddings.0` [1, 32, 256,
    256], `.1` [1, 64, 128, 128], `.2` [1, 256, 64, 64].
  - `prompt_encoder_mask_decoder.onnx`: the three embeddings, `input_points` [1, 1, N, 2] in
    1024 × 1024 coordinates, `input_labels` int64 [1, 1, N] (1 include, 0 exclude) and
    `input_boxes` [1, 0, 4] (none) → `pred_masks` [1, 1, 3, 256, 256] (scores; above 0 is
    selected) and `iou_scores` [1, 1, 3]; Gumka keeps the best of the three.
- Any number of clicks (the click count is a dynamic axis).
- Checked against darktable's own export of the same checkpoint (samexporter-style, with the
  padding point): identical masks for one click, two, and an exclusion.

## RealPLKSR (ONNX, Linux)

- darktable's ONNX export of the same MSSIM pretrain checkpoints as the Core ML conversion,
  from the repository those checkpoints are mirrored in.
- Inputs: `input` float32 [1, 3, 512, 512] (2×) or [1, 3, 256, 256] (4×), planar RGB from 0
  to 1. Output: `output` float32 [1, 3, 1024, 1024] for both. The tile size comes from the
  model, so the 4× upscale takes more, smaller tiles than on macOS.

## Candidates and exclusions

- Later, a fast second erase model: MI-GAN (Picsart, MIT code and weights, 512 × 512, about
  14 MB). Convert it ourselves from the official PyTorch export and check it against their ONNX
  pipeline; the third-party Core ML conversions (`ewchampion/migan-coreml-*`) have no model card
  and give a grid artifact over the whole output.
- MAT is CC BY-NC (research only): not usable.
- darktable's SAM 2.1 export (encoder and decoder, samexporter-style) is published only inside
  its `.dtmodel` archive; per-file hosting would allow pinning it in place of onnx-community's.
