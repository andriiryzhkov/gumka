// SPDX-License-Identifier: GPL-3.0-or-later

//! PNG reading and writing through the png crate, which gives every chunk: the profile, EXIF,
//! the resolution and text, as well as the pixels.
//!
//! XMP travels in an iTXt chunk ("XML:com.adobe.xmp"); IPTC and Photoshop resources as
//! ImageMagick's hex "Raw profile type iptc / 8bim" text, which exiftool reads too. Other text
//! chunks are copied as they are.

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufReader, Write};
use std::path::Path;

use png::text_metadata::{ITXtChunk, TEXtChunk, ZTXtChunk};
use png::{
    BitDepth, ColorType, PixelDimensions, ScaledFloat, SourceChromaticities, SrgbRenderingIntent,
    Transformations, Unit,
};

use crate::Rgb16;
use crate::meta::{self, Density, DensityUnit, Exif, Metadata};
use crate::picture::{Decoded, Error, Image, Result, gray_value, to_u8};

const XMP_KEY: &str = "XML:com.adobe.xmp";
const RAW_PROFILE: &str = "Raw profile type ";

/// What only PNG holds, kept for saving as PNG again.
#[derive(Default)]
pub(crate) struct PngExtras {
    latin1: Vec<TEXtChunk>,
    compressed: Vec<ZTXtChunk>,
    utf8: Vec<ITXtChunk>,
    /// Color chunks; written when there is no ICC profile, which overrides them.
    srgb: Option<SrgbRenderingIntent>,
    gamma: Option<ScaledFloat>,
    chromaticities: Option<SourceChromaticities>,
}

pub(crate) fn read(path: &Path) -> Result<Decoded> {
    // Metadata chunks count against png's limit (64 MB by default); the picture is the limit.
    let limits = png::Limits { bytes: usize::MAX };
    let mut decoder = png::Decoder::new_with_limits(BufReader::new(File::open(path)?), limits);
    decoder.set_transformations(Transformations::EXPAND);
    decoder.set_ignore_text_chunk(false);
    let mut reader = decoder.read_info()?;
    let size = reader
        .output_buffer_size()
        .ok_or_else(|| Error::Unsupported("PNG is too large".into()))?;
    let mut buf = vec![0; size];
    let frame = reader.next_frame(&mut buf)?;
    // Chunks after the image data, such as text. An animated PNG's other frames are skipped.
    if let Err(e) = reader.finish() {
        eprintln!("PNG chunks after the image left out: {e}");
    }

    let channels = match frame.color_type {
        ColorType::Grayscale => 1,
        ColorType::GrayscaleAlpha => 2,
        ColorType::Rgb => 3,
        ColorType::Rgba => 4,
        ColorType::Indexed => return Err(Error::Unsupported("unexpanded PNG palette".into())),
    };
    let sixteen = frame.bit_depth == BitDepth::Sixteen;
    let (w, h) = (frame.width, frame.height);
    let row = w as usize * channels * if sixteen { 2 } else { 1 };
    let mut samples = Vec::with_capacity(w as usize * h as usize * channels);
    for line in buf.chunks(frame.line_size).take(h as usize) {
        let line = &line[..row];
        if sixteen {
            samples.extend(
                line.as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| u16::from_be_bytes(*b)),
            );
        } else {
            samples.extend(line.iter().map(|&v| u16::from(v) * 257));
        }
    }
    let pixel = |p: &[u16]| match channels {
        1 | 2 => [p[0]; 3],
        _ => [p[0], p[1], p[2]],
    };
    let rgb: Vec<u16> = samples.chunks_exact(channels).flat_map(pixel).collect();
    let alpha = (channels % 2 == 0).then(|| {
        samples
            .chunks_exact(channels)
            .map(|p| p[channels - 1])
            .collect()
    });
    let pixels = Rgb16::from_raw(w, h, rgb)
        .ok_or_else(|| Error::Unsupported("PNG image data size mismatch".into()))?;

    Ok(Decoded {
        pixels,
        alpha,
        associated_alpha: false,
        bit_depth: if sixteen { 16 } else { 8 },
        gray: channels <= 2,
        meta: metadata(reader.info()),
        tiff: None,
    })
}

fn metadata(info: &png::Info) -> Metadata {
    let mut meta = Metadata {
        icc: info.icc_profile.as_ref().map(|p| p.to_vec()),
        // Some writers wrongly put JPEG's "Exif\0\0" before it.
        exif: info.exif_metadata.as_ref().and_then(|e| {
            let blob = e.strip_prefix(b"Exif\0\0").unwrap_or(e);
            Exif::from_blob(blob.to_vec())
        }),
        density: info.pixel_dims.map(|d| {
            let (x, y) = (f64::from(d.xppu), f64::from(d.yppu));
            // Pixels per meter; 300 dpi comes back as 11811, which is read as 300 again.
            let (dx, dy) = (x * 0.0254, y * 0.0254);
            match d.unit {
                Unit::Meter if (dx - dx.round()).abs() < 0.01 && (dy - dy.round()).abs() < 0.01 => {
                    Density {
                        x: dx.round(),
                        y: dy.round(),
                        unit: DensityUnit::Inch,
                    }
                }
                Unit::Meter => Density {
                    x: x / 100.0,
                    y: y / 100.0,
                    unit: DensityUnit::Centimeter,
                },
                Unit::Unspecified => Density {
                    x,
                    y,
                    unit: DensityUnit::Ratio,
                },
            }
        }),
        ..Default::default()
    };
    meta.png.srgb = info.srgb;
    meta.png.gamma = info.source_gamma;
    meta.png.chromaticities = info.source_chromaticities;

    // Text that is metadata goes to its place; the rest is kept as it is. A raw profile of a
    // kind that has a place is always taken, so that none is written back as text (with a
    // thumbnail, say): the first one that reads is used, and later copies are dropped.
    let mut photoshop = None;
    let mut take = |keyword: &str, text: Option<String>| -> bool {
        let Some(name) = keyword
            .strip_prefix(RAW_PROFILE)
            .map(str::to_ascii_lowercase)
        else {
            if keyword == XMP_KEY
                && let Some(text) = text
            {
                meta.xmp = Some(text.into_bytes());
                return true;
            }
            return false;
        };
        if !["iptc", "8bim", "xmp", "exif", "app1"].contains(&name.as_str()) {
            return false;
        }
        let Some(data) = text.as_deref().and_then(meta::decode_raw_profile) else {
            eprintln!("PNG {keyword:?} left out: unreadable");
            return true;
        };
        match name.as_str() {
            // Some writers put a whole Photoshop block under "iptc".
            "iptc" if data.starts_with(b"8BIM") => _ = photoshop.get_or_insert(data),
            "iptc" => _ = meta.iptc.get_or_insert(data),
            "8bim" => _ = photoshop.get_or_insert(data),
            "xmp" => _ = meta.xmp.get_or_insert(data),
            _ if meta.exif.is_none() => {
                let blob = data.strip_prefix(b"Exif\0\0").unwrap_or(&data);
                meta.exif = Exif::from_blob(blob.to_vec());
            }
            _ => {}
        }
        true
    };
    let mut latin1 = Vec::new();
    for chunk in &info.uncompressed_latin1_text {
        if !take(&chunk.keyword, Some(chunk.text.clone())) {
            latin1.push(chunk.clone());
        }
    }
    let mut compressed = Vec::new();
    for chunk in &info.compressed_latin1_text {
        let mut text = chunk.clone();
        let text = text
            .decompress_text()
            .ok()
            .and_then(|()| text.get_text().ok());
        if !take(&chunk.keyword, text) {
            compressed.push(chunk.clone());
        }
    }
    let mut utf8 = Vec::new();
    for chunk in &info.utf8_text {
        let mut text = chunk.clone();
        let text = text
            .decompress_text()
            .ok()
            .and_then(|()| text.get_text().ok());
        if !take(&chunk.keyword, text) {
            utf8.push(chunk.clone());
        }
    }
    meta.png.latin1 = latin1;
    meta.png.compressed = compressed;
    meta.png.utf8 = utf8;
    meta.photoshop = photoshop.and_then(meta::photoshop_without_thumbnails);
    if meta.iptc.is_none() {
        meta.iptc = meta.photoshop.as_deref().and_then(meta::photoshop_iptc);
    }
    meta
}

/// A color premultiplied by `alpha` as a straight one, rounded; black where it is fully
/// transparent.
fn unpremultiply(color: [u16; 3], alpha: u16) -> [u16; 3] {
    let a = u32::from(alpha);
    if a == 0 {
        return [0; 3];
    }
    color.map(|c| ((u32::from(c) * 65535 + a / 2) / a).min(65535) as u16)
}

/// Writes `image` as PNG, with as much of `meta` as PNG holds.
pub(crate) fn write<W: Write>(out: W, image: &Image, meta: &Metadata) -> Result<()> {
    let (w, h) = image.pixels.dimensions();
    let mut info = png::Info::with_size(w, h);
    info.color_type = match (image.gray, image.alpha.is_some()) {
        (true, false) => ColorType::Grayscale,
        (true, true) => ColorType::GrayscaleAlpha,
        (false, false) => ColorType::Rgb,
        (false, true) => ColorType::Rgba,
    };
    let sixteen = image.bit_depth == 16;
    info.bit_depth = if sixteen {
        BitDepth::Sixteen
    } else {
        BitDepth::Eight
    };
    info.icc_profile = meta.icc.as_deref().map(Cow::Borrowed);
    if meta.icc.is_none() {
        info.srgb = meta.png.srgb;
        info.source_gamma = meta.png.gamma;
        info.source_chromaticities = meta.png.chromaticities;
    }
    let exif = match meta.exif.as_ref().map(Exif::to_blob) {
        Some(Ok(mut blob)) => {
            if image.resized {
                meta::set_pixel_dimensions(&mut blob, w, h);
            }
            Some(blob)
        }
        Some(Err(e)) => {
            eprintln!("EXIF left out: {e}");
            None
        }
        None => None,
    };
    info.exif_metadata = exif.as_deref().map(Cow::Borrowed);
    info.pixel_dims = meta.density().map(|d| {
        let (scale, unit) = match d.unit {
            DensityUnit::Ratio => (1.0, Unit::Unspecified),
            DensityUnit::Inch => (1.0 / 0.0254, Unit::Meter),
            DensityUnit::Centimeter => (100.0, Unit::Meter),
        };
        let (xppu, yppu) = d.rounded(scale);
        PixelDimensions { xppu, yppu, unit }
    });
    info.uncompressed_latin1_text = meta.png.latin1.clone();
    info.compressed_latin1_text = meta.png.compressed.clone();
    info.utf8_text = meta.png.utf8.clone();
    if let Some(xmp) = &meta.xmp {
        match meta::xmp_text(xmp) {
            Some(text) => info.utf8_text.push(ITXtChunk::new(XMP_KEY, text)),
            None => eprintln!("XMP left out: it isn't text"),
        }
    }
    if let Some(iptc) = &meta.iptc {
        let text = meta::encode_raw_profile("iptc", iptc);
        info.compressed_latin1_text
            .push(ZTXtChunk::new(format!("{RAW_PROFILE}iptc"), text));
    }
    // Without its IPTC, which is above, so that readers don't find the records twice.
    let photoshop = match (&meta.photoshop, &meta.iptc) {
        (Some(block), Some(_)) => meta::photoshop_without_iptc(block),
        (block, _) => block.clone(),
    };
    if let Some(photoshop) = photoshop {
        let text = meta::encode_raw_profile("8bim", &photoshop);
        info.compressed_latin1_text
            .push(ZTXtChunk::new(format!("{RAW_PROFILE}8bim"), text));
    }

    let rgb = image.pixels.as_raw().as_chunks::<3>().0;
    let mut samples: Vec<u16> = Vec::with_capacity(rgb.len() * 4);
    for (i, &p) in rgb.iter().enumerate() {
        // PNG's alpha is straight: premultiplied colors are divided by their alpha.
        let p = match image.alpha {
            Some(alpha) if image.associated_alpha => unpremultiply(p, alpha[i]),
            _ => p,
        };
        if image.gray {
            samples.push(gray_value(p));
        } else {
            samples.extend_from_slice(&p);
        }
        if let Some(alpha) = image.alpha {
            samples.push(alpha[i]);
        }
    }
    let data: Vec<u8> = if sixteen {
        samples.iter().flat_map(|v| v.to_be_bytes()).collect()
    } else {
        samples.iter().map(|&v| to_u8(v)).collect()
    };
    let mut writer = png::Encoder::with_info(out, info)?.write_header()?;
    writer.write_image_data(&data)?;
    writer.finish()?;
    Ok(())
}
