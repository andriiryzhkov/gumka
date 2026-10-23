# Color

Gumka never changes a picture's colors: the stored values are what gets saved, with the file's
own profile. Color conversion happens only for showing the picture, and for the model when the
picture's encoding would mislead it. Conversions use Little CMS (`lcms2`).

## Display

egui textures are 8-bit. The full 16-bit image is kept for processing; the canvas converts its
preview levels with `gumka_core::color::DisplayTransform`:

- from the picture's ICC profile (sRGB when there is none or it can't be used), relative
  colorimetric with black point compensation, 16-bit in, 8-bit out, rows split over the cores
  (about 70 ms for 48 MP);
- gray pictures go through a 65,536-entry table computed without lcms2's optimization, which
  loses linear and gray shadows (about 5 ms to build; exact);
- the status bar names the profile: a profile with no readable name is "Unnamed color profile",
  or "Unreadable color profile (shown as sRGB)" when it can't be used. Names are read in English
  through lcms2.

Per platform:

- macOS (done): the preview is converted to Display P3 and the window's Metal layer is tagged
  Display P3 (`gumka-app/src/display.rs`), so macOS matches it to the actual display. Untagged,
  the layer's values would go to the display unconverted. egui-wgpu clears the tag on every
  surface configure (each resize), so a key-value observer on the layer's `colorspace` puts it
  back. UI colors are the design's sRGB values used as they are, so they show as Display P3.
- Windows (not yet; shown as sRGB): read the monitor ICC profile via Win32.
- Linux (not yet; shown as sRGB): the monitor profile from colord.

## The model's color space

Models get the picture's stored values as if they were sRGB, in the picture's own gamma-encoded
working space (Adobe RGB, Display P3, ...). That avoids clipping saturated colors, and removal
models mostly continue texture and structure, so the misinterpretation barely shows.

Linear and very wide pictures are different (`color::ModelSpace`):

- Flagged: a profile whose mid-gray gamma is under 1.5 (linear), or a matrix profile whose
  primaries' xy triangle exceeds 0.18 (Rec. 2020, ProPhoto, ACEScg; Adobe RGB and P3 are under
  0.15).
- A profile with any table (A2B/B2A/D2B tags: camera profiles, including those that also carry a
  matrix, which lcms2 leaves by the table and enters by the matrix) is judged by its curve only.
- For a flagged profile, the scaled crop is converted to Display P3 16-bit (relative colorimetric
  + BPC) for the model, and the fill is converted back before blending. Untouched pixels are
  never converted, and the feathered band is blended in the picture's own encoding.
- The conversion is unoptimized (lcms2's optimization loses linear and gray shadows), and it must
  give back a few neutrals and mild colors within 2%. A profile that can't convert both ways, or
  fails that check, keeps its values as they are, with one log line per profile. Unoptimized
  costs more per pixel, so large conversions (Upscale's tiles, at the model's output size too)
  are split over the cores by rows.
- Colors outside P3 are clipped in what the model sees, so a fill next to them can come back
  slightly less saturated.
- Upscale converts every tile, so for these profiles colors outside P3 are clipped across the
  whole photo, not only near a fill. Keeping them is a later step: a residual split, or soft
  clipping that can be undone (ITU-R BT.2407, Annex 5).
- The Upscale inspector says that colors beyond P3 may be clipped
  (`color::may_clip_beyond_p3`) when the profile is converted and its primaries are wider than
  P3, or can't be told: a linear profile that converts through a table (a camera profile) is
  judged by its curve only, so its gamut is unknown. A linear matrix profile that P3 holds
  (linear sRGB) and gray lose nothing, and a profile that fails the round trip isn't converted
  at all. The app works this out the first time the inspector shows, once per photo.
- Of the 1,560 profiles Capture One ships, only ROMM (ProPhoto) and Phase One's linear gray are
  converted; its camera profiles are tables and keep their values.
- Cloud APIs (later) assume 8-bit sRGB: convert the crop to sRGB properly for them.
