# Image I/O and metadata

TIFF, PNG and JPEG, read and written in pure Rust on every OS: the `tiff` crate for 16-bit
TIFF, `png` directly, `image` (zune-jpeg) for JPEG pixels with our own segment walker for JPEG
metadata. Every picture is held as 16-bit RGB (`Rgb16`) with optional alpha; grayscale pictures
are held with R = G = B and written back as grayscale.

## The metadata record

`gumka-core/src/meta.rs` holds one record, read from and written to every format as far as the
format holds it:

| Metadata | TIFF | JPEG | PNG |
|---|---|---|---|
| ICC profile | 34675 | APP2 | iCCP |
| EXIF with GPS | tags and sub-directories | APP1 | eXIf |
| XMP | 700 | APP1 | iTXt |
| IPTC | see below | see below | see below |
| Photoshop resources | 34377 | APP13 | "Raw profile type 8bim" |
| Resolution | 282 / 283 / 296 | JFIF | pHYs |

- PNG text chunks and JPEG comments stay in their own format.
- Byte streams are copied without interpretation, in the file's byte order (ICC, XMP, IPTC and
  Photoshop resources keep the source's byte order and declared type).
- A JPEG/PNG EXIF blob is kept byte for byte into JPEG and PNG, so camera maker notes survive
  there; `tiff_io` converts it to and from TIFF tags when crossing to or from TIFF.
- Thumbnails are dropped on every save, so the erased object can't survive in them: EXIF IFD1
  (unlinked and blanked in place in a kept blob), Photoshop resources 1033, 1036, 1058 and 1059.
- In JPEG output, Exif, XMP, extended XMP, the Photoshop block and comments go right after JFIF;
  every segment's length is checked, and a Photoshop block too big for one segment is split at
  resource boundaries.
- XMP too large for one JPEG segment loses its padding, then `photoshop:DocumentAncestors`; if
  it still doesn't fit, it is left out of that JPEG with a log line.

## Where IPTC goes

- TIFF → TIFF keeps tag 33723 and the Photoshop block as read, less its thumbnails (with its
  0x0404 copy and 0x0425 digest, as Photoshop writes them).
- JPEG holds IPTC only in the Photoshop block (APP13 resource 0x0404); a 0x0404 made for it gets
  no digest.
- PNG, and a TIFF made from JPEG or PNG, hold it apart (PNG "Raw profile type iptc", TIFF 33723),
  with 0x0404 and 0x0425 taken out of the block.

## Resolution

A TIFF always states its resolution with a unit:

- inches where the source's resolution had no unit (TIFF's default);
- 72 pixels per inch where the source had no resolution, or only said "square pixels" (no unit,
  equal values: JFIF's and pHYs's usual 1:1, and a TIFF's own 1/1 as the tiff crate and earlier
  Gumka builds wrote it). That is what readers assume; 1/1 with no unit reads as 1 dpi on macOS.
- Pixels that aren't square keep their ratio.

pHYs (pixels per meter) is read back as whole dpi where it is one (11811 → 300).

## Size changes

After an upscale the picture is larger than the file it came from. The pixel layout of every
format is written from the pixels, so it is always right. Of the metadata:
- **EXIF's record of the size is rewritten when the size changed:** the EXIF directory's
  PixelXDimension and PixelYDimension (40962, 40963), and in JPEG and PNG EXIF also the main
  directory's width and length (256, 257).
  - JPEG and PNG EXIF is patched in place, so the maker note's offsets stay valid. A value is
    changed only where the tag holds one SHORT or LONG that the new value fits; a malformed
    EXIF is left as it is.
  - A picture whose size didn't change keeps EXIF's values as they were, even if they were
    already wrong.
- **The resolution is kept**, as Photoshop does when it resamples: 300 dpi stays 300 dpi and the
  print size grows.
- **Values that still describe the old size:**
  - XMP's size fields (`tiff:ImageWidth`, `exif:PixelXDimension`);
  - Photoshop resources that hold pixel positions (guides, slices).
- **A TIFF can't go past 4 GB.** Saving one that would says to save as PNG instead.

## Alpha

- TIFF holds either kind of alpha: straight (unassociated, ExtraSamples 2) or premultiplied
  (associated, ExtraSamples 1, or the older Matteing). A TIFF saved as TIFF keeps its kind and
  its values as they are.
- PNG holds straight alpha only. A premultiplied alpha is written straight: each color times
  65535 over its alpha, rounded, at 16 bits before an 8-bit file's rounding; black where the
  alpha is 0. Written as it is, its soft edges would come out dark.
- JPEG has no alpha, and the stored colors are written. For premultiplied alpha that is the
  picture over black, a sensible flattening; turning them straight would bring out colors
  hidden under transparent pixels. A straight alpha's colors are written as they are too.

## Odd and malformed input

Files are untrusted input: parsing must never panic, hang or allocate what a file merely claims.

- An EXIF blob that isn't a TIFF structure that reads is dropped on open, so no save copies it.
- PNG raw profiles of a kind with a home (exif, app1, xmp, iptc, 8bim) are always taken out of
  the text: the first copy that reads is used, later and unreadable copies are dropped; unknown
  ones stay as text.
- A broken, looping or too-deep sub-directory (EXIF, GPS, Interop) is dropped instead of failing
  the open.
- A TIFF tag value or EXIF value that claims more bytes than its file holds is left out, never
  allocated: once tiff has read the first directory (with its default limits), the limits are
  the file's size, raised to the picture's size only while the pixels are decoded.
- An ICC profile too short to name its color space is dropped, and a profile whose color space
  doesn't match the pixels is dropped (a gray picture with an RGB profile is saved as RGB).
- TIFF Matteing (32995) is read as associated alpha.

## Refused

CMYK JPEGs, planar TIFFs, 1-, 2- and 4-bit gray, white-is-zero gray with alpha, and TIFFs with
an extra channel that isn't alpha are refused with a clear message, never silently changed.

## Known limits

- A maker note can break when EXIF is re-laid out (TIFF → anything, and JPEG/PNG → TIFF).
- Maker-note previews and `xmp:Thumbnails` still hold the old picture.
- XMP is copied untouched, so values describing the file (bit depth, compression, size) can go
  stale, the size after an upscale included; extended XMP stays JPEG-only.
- Premultiplied (associated) alpha is kept, but the erased area isn't premultiplied.
