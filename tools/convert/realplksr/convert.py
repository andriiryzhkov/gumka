# SPDX-License-Identifier: GPL-3.0-or-later
"""Converts the RealPLKSR 2x and 4x upscalers to Core ML packages for Gumka.

The checkpoints are the "MSSIM pretrain" weights (trained without GAN fine-tuning) that
neosr-project released under MIT in dslisleedh/PLKSR issue #4, fetched from darktable's mirror
at a pinned revision and checked by SHA-256. Each is converted with float32 tensor input and
output and float16 compute, at the tile size Gumka runs it: 512 px in, so 1024 px out for 2x
and 2048 px out for 4x.

The script then checks the packages against PyTorch on generated tiles, runs a tiled image the
way Gumka does to measure seams, writes a model card and license for the upload, and prints
the `[[models.files]]` entries for models.toml.

The PLKConv2d patch follows darktable-ai's convert.py (GPL-3.0).

Usage: uv run convert.py [--out DIR] [--no-verify]
"""

import argparse
import hashlib
import sys
import time
import urllib.request
from pathlib import Path

import coremltools as ct
import numpy as np
import torch
from spandrel import ImageModelDescriptor, ModelLoader

MIRROR = (
    "https://huggingface.co/darktable-org/upscale-realplksr-onnx/resolve/"
    "0bb636eeeed5fffd256571c3cc440d9fdf62467e/checkpoints/"
)
# scale: (checkpoint, size, SHA-256, input tile side)
CHECKPOINTS = {
    2: (
        "2x_realplksr_mssim_pretrain.pth",
        29_595_266,
        "0214b82d20a2b7d524c9f70828c7a9b0113a3058ff3eb3cabe2b11783645c381",
        512,
    ),
    4: (
        "4x_realplksr_mssim_pretrain.pth",
        29_678_402,
        "37a1a6245ea29dc17655b1df64ba5090b0d5ed9d4b50525a722c53e378d552f1",
        512,
    ),
}
# Input pixels of context on each side of a tile, as Gumka and darktable use.
OVERLAP = 16
# Core ML on CPU and GPU against PyTorch in float32, per tile. The Neural Engine is reported but
# not required: Gumka runs these models on CPU and GPU.
MIN_PSNR = 40.0

SOURCE = "https://github.com/dslisleedh/PLKSR/issues/4"
AUTHORS = "PLKSR: Dongheon Lee et al. RealPLKSR and its weights: neosr-project."
COPYRIGHT = "Copyright (c) 2024 Dongheon Lee"
HERE = Path(__file__).resolve().parent
MIT_TEXT = HERE.parents[2] / "assets" / "licenses" / "MIT.txt"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def fetch(name: str, size: int, digest: str, folder: Path) -> Path:
    """Downloads a checkpoint once, refusing anything but the pinned file."""
    path = folder / name
    if path.exists() and path.stat().st_size == size and sha256(path) == digest:
        return path
    folder.mkdir(parents=True, exist_ok=True)
    part = path.with_suffix(".part")
    print(f"Downloading {name}")
    urllib.request.urlretrieve(MIRROR + name, part)
    if part.stat().st_size != size or sha256(part) != digest:
        part.unlink()
        sys.exit(f"{name}: size or SHA-256 doesn't match the pinned checkpoint")
    part.rename(path)
    return path


def patch_plkconv() -> None:
    """Makes PLKConv2d use its split+cat path in eval mode too.

    The eval path assigns into a slice of its input in place, which tracing turns into a
    scatter. split+cat is numerically identical and converts to plain ops.
    """
    from spandrel.architectures.PLKSR.__arch.RealPLKSR import PLKConv2d

    def forward(self, x: torch.Tensor) -> torch.Tensor:
        x1, x2 = torch.split(x, [self.idx, x.size(1) - self.idx], dim=1)
        return torch.cat([self.conv(x1), x2], dim=1)

    PLKConv2d.forward = forward


def load(path: Path, scale: int) -> torch.nn.Module:
    descriptor = ModelLoader().load_from_file(path)
    if not isinstance(descriptor, ImageModelDescriptor):
        sys.exit(f"{path.name}: not an image model")
    arch = descriptor.architecture.id
    if "PLKSR" not in arch or descriptor.scale != scale:
        sys.exit(f"{path.name}: {arch} x{descriptor.scale}, expected RealPLKSR x{scale}")
    if descriptor.input_channels != 3 or descriptor.output_channels != 3:
        sys.exit(f"{path.name}: expected RGB in and out")
    model = descriptor.model.eval()
    from spandrel.architectures.__arch_helpers.dysample import DySample

    dysample = any(isinstance(m, DySample) for m in model.modules())
    params = sum(p.numel() for p in model.parameters())
    upsampler = "DySample" if dysample else "pixel shuffle"
    print(f"  {arch} x{scale}, {params:,} parameters, {upsampler} upsampler")
    return model


def convert(model: torch.nn.Module, scale: int, tile: int, digest: str) -> ct.models.MLModel:
    example = torch.rand(1, 3, tile, tile)
    with torch.no_grad():
        traced = torch.jit.trace(model, example, check_trace=False)
    mlmodel = ct.convert(
        traced,
        inputs=[ct.TensorType(name="input", shape=(1, 3, tile, tile), dtype=np.float32)],
        outputs=[ct.TensorType(name="output", dtype=np.float32)],
        convert_to="mlprogram",
        compute_precision=ct.precision.FLOAT16,
        minimum_deployment_target=ct.target.macOS13,
    )
    out = tile * scale
    mlmodel.author = AUTHORS
    mlmodel.license = f"MIT. {COPYRIGHT}."
    mlmodel.short_description = (
        f"RealPLKSR {scale}x (MSSIM pretrain). Upscales a {tile}x{tile} RGB tile to {out}x{out}."
    )
    mlmodel.version = "1.0"
    mlmodel.input_description["input"] = f"RGB, planar [1, 3, {tile}, {tile}], values 0 to 1"
    mlmodel.output_description["output"] = f"RGB, planar [1, 3, {out}, {out}], values 0 to 1"
    meta = mlmodel.user_defined_metadata
    meta["source"] = SOURCE
    meta["checkpoint"] = CHECKPOINTS[scale][0]
    meta["checkpoint_sha256"] = digest
    meta["scale"] = str(scale)
    meta["tile"] = str(tile)
    meta["overlap"] = str(OVERLAP)
    return mlmodel


def test_tiles(tile: int) -> dict[str, np.ndarray]:
    """Generated inputs: a smooth gradient, hard edges, noise and fine texture."""
    rng = np.random.default_rng(7)
    y, x = np.mgrid[0:tile, 0:tile] / (tile - 1)
    tiles = {
        "gradient": np.stack([x, y, (x + y) / 2]),
        "edges": np.stack([(np.floor(x * 16) + np.floor(y * 16)) % 2] * 3) * 0.8 + 0.1,
        "noise": 0.5 + rng.normal(0, 0.15, (3, tile, tile)),
        "texture": 0.5
        + 0.3
        * np.stack(
            [
                np.sin(90 * x + 6 * np.sin(17 * y)),
                np.sin(120 * y + 5 * np.cos(13 * x)),
                np.sin(70 * (x + y)),
            ]
        ),
    }
    return {k: np.clip(v, 0, 1)[None].astype(np.float32) for k, v in tiles.items()}


def psnr(a: np.ndarray, b: np.ndarray) -> float:
    mse = float(np.mean((np.clip(a, 0, 1) - np.clip(b, 0, 1)) ** 2))
    return float("inf") if mse == 0 else 10 * np.log10(1 / mse)


def mirror(i: np.ndarray, n: int) -> np.ndarray:
    """Reflect-101 indices, as Gumka reads beyond the image edge."""
    if n == 1:
        return np.zeros_like(i)
    period = 2 * (n - 1)
    m = np.mod(i, period)
    return np.where(m >= n, period - m, m)


def tiled(predict, image: np.ndarray, scale: int, tile: int) -> np.ndarray:
    """Runs `predict` over `image` in tiles the way Gumka does: each tile reads its core plus
    OVERLAP pixels of context per side, and only the core is written."""
    _, _, h, w = image.shape
    core = tile - 2 * OVERLAP
    out = np.zeros((1, 3, h * scale, w * scale), np.float32)
    for y0 in range(0, h, core):
        for x0 in range(0, w, core):
            ys = mirror(np.arange(y0 - OVERLAP, y0 - OVERLAP + tile), h)
            xs = mirror(np.arange(x0 - OVERLAP, x0 - OVERLAP + tile), w)
            result = predict(image[:, :, ys][:, :, :, xs])
            ch, cw = min(core, h - y0), min(core, w - x0)
            o = OVERLAP * scale
            out[:, :, y0 * scale : (y0 + ch) * scale, x0 * scale : (x0 + cw) * scale] = result[
                :, :, o : o + ch * scale, o : o + cw * scale
            ]
    return out


def verify(model: torch.nn.Module, package: Path, scale: int, tile: int) -> bool:
    ok = True
    reference = {}
    with torch.no_grad():
        for name, t in test_tiles(tile).items():
            reference[name] = model(torch.from_numpy(t)).numpy()
    for units in (ct.ComputeUnit.CPU_AND_GPU, ct.ComputeUnit.ALL):
        ml = ct.models.MLModel(str(package), compute_units=units)

        def predict(t: np.ndarray) -> np.ndarray:
            return ml.predict({"input": t})["output"]

        for name, t in test_tiles(tile).items():
            got = predict(t)
            p = psnr(got, reference[name])
            worst = float(np.abs(got - reference[name]).max())
            print(f"  {units.name:12} {name:9} PSNR {p:6.1f} dB, max error {worst:.4f}")
            if units == ct.ComputeUnit.CPU_AND_GPU:
                ok &= p >= MIN_PSNR
        t = test_tiles(tile)["texture"]
        for _ in range(2):
            predict(t)
        times = []
        for _ in range(5):
            start = time.perf_counter()
            predict(t)
            times.append(time.perf_counter() - start)
        print(f"  {units.name:12} {1000 * float(np.median(times)):.0f} ms per tile")

    # Seams: a picture three tiles wide, tiled through Core ML against PyTorch run on the
    # whole picture at once. Tiles see only OVERLAP pixels of context, so a difference that
    # concentrates at the tile borders is a seam. The picture's own edges are left out: there
    # the tiles mirror the picture while PyTorch pads with zeros.
    core = tile - 2 * OVERLAP
    h, w = core // 2 + 40, 2 * core + core // 3
    y, x = np.mgrid[0:h, 0:w]
    smooth = np.stack([x / w, y / h, 0.5 + 0.05 * np.sin(x / 9.0) * np.cos(y / 7.0)])
    image = np.clip(smooth, 0, 1)[None].astype(np.float32)
    ml = ct.models.MLModel(str(package), compute_units=ct.ComputeUnit.CPU_AND_GPU)
    got = tiled(lambda t: ml.predict({"input": t})["output"], image, scale, tile)
    with torch.no_grad():
        whole = model(torch.from_numpy(image)).numpy()
    edge = 2 * OVERLAP * scale
    diff = np.abs(got - whole)[:, :, edge:-edge, edge:-edge]
    error = diff.max(axis=(0, 1, 2))
    near = np.zeros(error.shape, bool)
    for border in range(core * scale, w * scale, core * scale):
        x = border - edge
        near[max(x - 4 * scale, 0) : x + 4 * scale] = True
    print(
        f"  seams: PSNR {psnr(got[:, :, edge:-edge, edge:-edge], whole[:, :, edge:-edge, edge:-edge]):.1f} dB "
        f"inside the picture; max error {error[near].max():.4f} at tile borders, "
        f"{error[~near].max():.4f} elsewhere"
    )
    return ok


def model_card(out: Path) -> None:
    """The README and LICENSE that go to the Hugging Face repository with the packages."""
    rows = []
    for scale, (checkpoint, _, digest, tile) in CHECKPOINTS.items():
        rows.append(
            f"| `RealPLKSR-{scale}x.mlpackage` | {scale}x | [1, 3, {tile}, {tile}] float32 | "
            f"[1, 3, {tile * scale}, {tile * scale}] float32 | `{checkpoint}` (SHA-256 "
            f"`{digest[:12]}…`) |"
        )
    table = "\n".join(rows)
    (out / "README.md").write_text(
        f"""---
license: mit
library_name: coreml
pipeline_tag: image-to-image
tags:
  - super-resolution
  - coreml
---

# RealPLKSR for Core ML

Core ML conversions of the RealPLKSR "MSSIM pretrain" upscalers, 2x and 4x, as used by
[Gumka](https://github.com/andriiryzhkov/gumka). These checkpoints were trained without GAN
fine-tuning: they sharpen what is in the picture rather than inventing detail.

| Package | Scale | Input | Output | Checkpoint |
|---|---|---|---|---|
{table}

- Values are planar RGB from 0 to 1, in and out. Compute is float16; input and output are
  float32, so 16-bit pictures keep their precision.
- The tile size is fixed. Larger pictures are run in tiles with {OVERLAP} px of context on
  each side, writing only each tile's core.
- Run them on CPU and GPU. On the Neural Engine the results are inaccurate.
- About 1.3 s (2x) and 1.5 s (4x) per tile on an M4 with CPU and GPU.
- ML Program, macOS 13 or later.

## Source and license

- Architecture: PLKSR, "Partial Large Kernel CNNs for Efficient Super-Resolution", Dongheon
  Lee et al., <https://github.com/dslisleedh/PLKSR> (MIT).
- RealPLKSR and these weights: neosr-project, released under the same MIT license in
  <{SOURCE}>.
- Checkpoints fetched from darktable's mirror,
  <https://huggingface.co/darktable-org/upscale-realplksr-onnx>, and checked by SHA-256.
- The weights are stored as float16; nothing else changes. License: MIT, see `LICENSE`.

## Conversion

`tools/convert/realplksr/convert.py` in the Gumka repository: spandrel loads the checkpoint,
`torch.jit.trace` traces it at the tile size, and coremltools 9.0 converts it. The script
checks each package against PyTorch on generated tiles.
"""
    )
    (out / "LICENSE").write_text(f"MIT License\n\n{COPYRIGHT}\n\n{MIT_TEXT.read_text()}")


def files_entries(out: Path, package: Path) -> str:
    entries = []
    for path in sorted(p for p in package.rglob("*") if p.is_file()):
        entries.append(
            f'[[models.files]]\npath = "{path.relative_to(out).as_posix()}"\n'
            f"size = {path.stat().st_size}\nsha256 = \"{sha256(path)}\"\n"
        )
    return "\n".join(entries)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", type=Path, default=HERE / "out")
    parser.add_argument("--no-verify", action="store_true")
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)

    patch_plkconv()
    ok = True
    entries = []
    for scale, (name, size, digest, tile) in CHECKPOINTS.items():
        print(f"RealPLKSR {scale}x")
        model = load(fetch(name, size, digest, out / "checkpoints"), scale)
        package = out / f"RealPLKSR-{scale}x.mlpackage"
        convert(model, scale, tile, digest).save(str(package))
        print(f"  saved {package.name}")
        if not args.no_verify:
            ok &= verify(model, package, scale, tile)
        entries.append(files_entries(out, package))
    model_card(out)
    print("\nmodels.toml entries:\n")
    print("\n".join(entries))
    if not ok:
        sys.exit(f"Core ML differs from PyTorch by more than {MIN_PSNR} dB PSNR allows")


if __name__ == "__main__":
    main()
