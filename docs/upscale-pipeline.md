# Upscale pipeline

`gumka_core::upscale` (`upscale`, `Factor`, `result_size`, `Alpha`, the `Upscaler` trait),
shared by the app's worker and the CLI. The model is RealPLKSR at 2× or 4×
([models.md](models.md)). Factors go from 1.25× to 4× in quarter steps: 2× and 4× are the
models' own output, and any other factor runs the next model up (up to 2× the 2× model, above it
the 4× model) and scales its output down.

1. Check the result's size: at most `MAX_PIXELS` (256 MP) and `MAX_SIDE` (65,535 px a side,
   JPEG's limit). `result_size` says why a factor is too large, in words for the user, and
   `largest_factor` gives the largest that fits (the app offers only that; the CLI suggests a
   smaller scale). The
   output is reserved with `try_reserve_exact`, so running out of memory is an error, not a
   crash.
2. Cut the picture into tiles. Each tile is the model's fixed input size (512 px).
   - Its core, the tile less `OVERLAP` (16 px) on every side, is the part it writes.
   - The cores cover the picture exactly once.
   - Each tile reads its core plus the overlap. Beyond the picture's edges it reads the
     picture mirrored without repeating the edge pixel (… 2 1 0 1 2 …), which also covers
     pictures smaller than one tile.
3. Feed the model each tile's values as floats from 0 to 1, planar RGB, in the picture's own
   encoding. Only linear and very wide profiles are converted to Display P3, tile by tile
   ([color.md](color.md#the-models-color-space)). Under straight alpha, fully transparent pixels
   get the colors of the nearest visible ones first, ring by ring for up to `OVERLAP` pixels:
   their own colors are hidden and can be anything (often black), and the model would blur them
   into the visible edge.
4. Take the core of the model's output (`scale` times larger), clip it to 0 to 1, convert it
   to 16 bits, convert it back to the picture's profile if step 3 converted it, and write it.
   - At another factor, each core's place in the result comes from one mapping for the whole
     picture (a position times result size over picture size, rounded), so neighbouring cores
     meet exactly.
   - Each core is resampled with Lanczos 3 (`fast_image_resize`) from the exact fractional part
     of the tile's output that it covers. The filter reads into the tile's context around that
     part, so tiles still join without seams.
   - A grayscale picture, where every pixel has equal channels, gets gray values.
   - An alpha channel is scaled with Catmull-Rom (`fast_image_resize`) on the same tiles, from
     the part of the tile its core covers, at any factor. It never goes through a whole-picture
     resize, which would allocate a float copy of the whole result.
   - Under premultiplied alpha (a TIFF's associated alpha), no color comes back above its
     alpha, in the picture's encoding: a sharpened edge would otherwise add light that isn't
     there.
5. Report progress after each tile. The caller can stop between tiles, which returns `Ok(None)`
   and leaves the picture as it was.

Invariants (tests in `upscale.rs` and `tests/upscale.rs`):
- **Seamless tiles:** with a model that ignores context (it only replicates pixels), the
  result is bit-identical to replicating the whole picture, at any size.
- **Enough context:** a model that sees one pixel of context gives exactly what it gives on the
  whole picture at once.
- **Faithful colors:** an ordinary profile's values reach the model unconverted. A linear
  picture comes back within 2%.
- **Fractional factors join seamlessly:** with the model that only replicates pixels, every
  factor between the models' own matches one resample of the whole picture within 1 in 65,535,
  except at the picture's edges, where tiles mirror the picture.
- **Full coverage:** a flat picture stays flat at every factor, every pixel included.
- **Alpha:** the tiles' alpha is one Catmull-Rom resample of the whole alpha within 1, away
  from the edges. Hidden colors under straight alpha don't reach visible pixels through a model
  that blurs, and premultiplied colors stay within their alpha through one that overshoots.

Measured with the real model, which sees far more than 16 px, against PyTorch on the whole
picture: errors at tile borders are no larger than anywhere else
(`tools/convert/realplksr/convert.py`).

Speed: the model does about 15 MFLOP per input pixel. On an M4 with CPU and GPU a tile takes
about 1.3 s (2×) or 1.5 s (4×), so a 24 MP photo takes 2 to 3 minutes. The Neural Engine gives
wrong results for this model and isn't meaningfully faster. A factor between the models' own
takes as long as the model it runs.

After an upscale the picture's size differs from the file's. Saving writes the new size into
EXIF and keeps the resolution, so the print size grows ([image-io.md](image-io.md#size-changes)).
