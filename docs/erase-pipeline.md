# Erase pipeline

`gumka_core::erase` (`ErasePlan`, `erase`), shared by the app's worker and the CLI.

1. Take the mask's bounding box, expanded about 2-3× for context (`EraseSettings::context`).
2. Scale the crop to the model's input size (800×800 for the current LaMa).
3. Feed the model the crop's values in the picture's own encoding, converted to Display P3 only
   for linear and very wide profiles ([color.md](color.md#the-models-color-space)).
4. Run the model and scale the result back to the crop's size (converted back to the picture's
   profile first when step 3 converted it).
5. Blend only the slightly grown, feathered mask region (`EraseSettings::grow` and `feather`,
   in model pixels) into the full 16-bit image. A crop that is all gray (a grayscale picture)
   gets a gray fill.
6. Pixels outside the blended region (the mask and its soft edge) must be bit-identical to the
   input. Non-negotiable. This holds in memory and in TIFF and PNG output; JPEG output is
   re-encoded as a whole.

`erase` returns the `Patch` it replaced; after `swap_region` the patch holds the old pixels,
which is what undo keeps.

LaMa's I/O is 8-bit, so the fill is 8-bit before it is blended into the 16-bit image. If that
shows banding in smooth areas, make our own conversion with float tensor I/O
([models.md](models.md#lama-core-ml)).
