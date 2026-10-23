# RealPLKSR to Core ML

Converts the RealPLKSR "MSSIM pretrain" upscalers (2× and 4×, MIT) into the Core ML packages
the Upscale tool uses. This isn't part of the app or its build. Run it by hand when the models
change, publish the result, then pin it in `models.toml`.

## Run

```sh
cd tools/convert/realplksr
uv run convert.py            # or --out DIR, --no-verify
```

`uv` installs the pinned Python and packages from `uv.lock` into `.venv` (about 450 MB). Then
the script:

1. Downloads the two checkpoints from darktable's mirror at a pinned revision and checks
   their SHA-256.
2. Converts each with spandrel, `torch.jit.trace` and coremltools to an ML Program:
   - float32 input `input` [1, 3, 512, 512] and output `output` [1, 3, 512·s, 512·s];
   - planar RGB from 0 to 1;
   - float16 compute;
   - macOS 13 or later.
3. Checks each package against PyTorch on generated tiles (gradient, edges, noise, texture).
   It fails below 40 dB PSNR on CPU and GPU. The Neural Engine is reported but not required,
   since Gumka runs these models on CPU and GPU.
4. Runs a picture three tiles wide the way Gumka tiles it (16 px of mirrored context per side,
   only the core written) and compares it with PyTorch on the whole picture, at the tile
   borders and elsewhere.
5. Writes `out/README.md` (model card) and `out/LICENSE`, and prints the `[[models.files]]`
   entries for `models.toml`.

Everything goes into `out/`, which git ignores.

Results on an M4 (October 2026), CPU and GPU:
- **Accuracy:** 72–75 dB PSNR against PyTorch, with no seams: errors at tile borders match the
  rest of the picture.
- **Speed:** about 1.3 s per tile for 2× and 1.5 s for 4×. The model does about 15 MFLOP per
  input pixel, so a 24 MP photo takes 2 to 3 minutes.
- **Neural Engine:** gives 29–36 dB and isn't meaningfully faster.

## Publish

1. Upload `RealPLKSR-2x.mlpackage`, `RealPLKSR-4x.mlpackage`, `README.md` and `LICENSE` from
   `out/` to the Hugging Face model repository,
   [gumka/realplksr-coreml](https://huggingface.co/gumka/realplksr-coreml). Leave out
   `checkpoints/`:

   ```sh
   hf upload gumka/realplksr-coreml tools/convert/realplksr/out . --exclude "checkpoints/*"
   ```

2. Note the commit the upload made.
3. In `models.toml`, set `base_url` to `https://huggingface.co/<repo>/resolve/<commit>/` and
   paste the printed file entries.

The weights and the generated `model.mlmodel` are reproducible, but `Manifest.json` gets new
identifiers on every run. So `models.toml` must list the files that were actually uploaded.

The PLKConv2d patch follows darktable-ai's `convert.py` (GPL-3.0).
