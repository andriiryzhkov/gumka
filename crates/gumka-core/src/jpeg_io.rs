// SPDX-License-Identifier: GPL-3.0-or-later

//! JPEG metadata: read from the segments before the image data, and spliced into what the
//! image crate's encoder writes. That encoder holds only JFIF, EXIF and the ICC profile, and
//! writes an EXIF over 64 KB with a wrapped length, so EXIF is spliced in here too.
//!
//! Only metadata segments are carried over: EXIF, XMP (with its extension), Photoshop
//! resources (with IPTC) and comments. Others, such as Adobe's color-transform marker or MPF
//! preview images, would not fit the new image.

use std::fs;
use std::io::Write;
use std::path::Path;

use image::codecs::jpeg::{JpegEncoder, PixelDensity, PixelDensityUnit};
use image::{DynamicImage, GrayImage, ImageEncoder, ImageFormat, RgbImage};

use crate::meta::{self, Density, DensityUnit, EXIF_DIRECTORY, Exif, Metadata};
use crate::picture::{Decoded, Error, Image, Result, gray_value, to_u8};
use crate::tiff_io;

/// JPEG output quality: high enough that re-encoding doesn't show.
const QUALITY: u8 = 95;

const APP0: u8 = 0xE0;
const APP1: u8 = 0xE1;
const APP2: u8 = 0xE2;
const APP13: u8 = 0xED;
const COM: u8 = 0xFE;
const SOS: u8 = 0xDA;
const EOI: u8 = 0xD9;

const EXIF_HEADER: &[u8] = b"Exif\0\0";
const XMP_HEADER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const XMP_EXTENSION_HEADER: &[u8] = b"http://ns.adobe.com/xmp/extension/\0";
const ICC_HEADER: &[u8] = b"ICC_PROFILE\0";
const PHOTOSHOP_HEADER: &[u8] = b"Photoshop 3.0\0";

/// The most a segment carries after its marker and length.
const MAX_SEGMENT: usize = 65533;

/// The EXIF tag of a maker note.
const MAKER_NOTE: u16 = 37500;

/// What only JPEG holds, kept for saving as JPEG again.
#[derive(Default)]
pub(crate) struct JpegExtras {
    /// COM segments.
    comments: Vec<Vec<u8>>,
    /// Extended XMP segments, whole: they go with the main XMP, which refers to them.
    xmp_extension: Vec<Vec<u8>>,
}

pub(crate) fn read(path: &Path) -> Result<Decoded> {
    let data = fs::read(path)?;
    // A frame header (SOF0-15, but DHT, JPG and DAC) gives the number of components.
    let components = segments(&data)
        .into_iter()
        .find(|&(m, ..)| (0xC0..=0xCF).contains(&m) && ![0xC4, 0xC8, 0xCC].contains(&m))
        .and_then(|(_, payload, _)| payload.get(5).copied());
    if components == Some(4) {
        return Err(Error::Unsupported(
            "CMYK JPEGs aren't supported: Gumka works on RGB and gray pictures".into(),
        ));
    }
    let image = image::load_from_memory_with_format(&data, ImageFormat::Jpeg)?;
    Ok(Decoded {
        gray: !image.color().has_color(),
        pixels: image.to_rgb16(),
        alpha: None,
        associated_alpha: false,
        bit_depth: 8,
        meta: metadata(&data),
        tiff: None,
    })
}

/// The segments before the image data, as (marker, payload, offset after the segment).
fn segments(data: &[u8]) -> Vec<(u8, &[u8], usize)> {
    let mut out = Vec::new();
    if data.get(0..2) != Some(&[0xFF, 0xD8]) {
        return out;
    }
    let mut at = 2;
    loop {
        // A marker may be preceded by fill bytes.
        while data.get(at) == Some(&0xFF) && data.get(at + 1) == Some(&0xFF) {
            at += 1;
        }
        let (Some(0xFF), Some(&marker)) = (data.get(at), data.get(at + 1)) else {
            break;
        };
        if marker == SOS || marker == EOI {
            break;
        }
        at += 2;
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue; // no length
        }
        let Some(len) = data
            .get(at..at + 2)
            .map(|b| usize::from(u16::from_be_bytes([b[0], b[1]])))
            .filter(|&len| len >= 2)
        else {
            break;
        };
        let Some(payload) = data.get(at + 2..at + len) else {
            break;
        };
        at += len;
        out.push((marker, payload, at));
    }
    out
}

fn metadata(data: &[u8]) -> Metadata {
    let mut meta = Metadata::default();
    let mut icc: Vec<(u8, &[u8])> = Vec::new();
    let mut photoshop: Option<Vec<u8>> = None;
    for (marker, payload, _) in segments(data) {
        match marker {
            APP0 if payload.starts_with(b"JFIF\0") && payload.len() >= 12 => {
                let x = u16::from_be_bytes([payload[8], payload[9]]);
                let y = u16::from_be_bytes([payload[10], payload[11]]);
                let unit = match payload[7] {
                    1 => DensityUnit::Inch,
                    2 => DensityUnit::Centimeter,
                    _ => DensityUnit::Ratio,
                };
                meta.density = (x > 0 && y > 0).then_some(Density {
                    x: f64::from(x),
                    y: f64::from(y),
                    unit,
                });
            }
            APP1 if payload.starts_with(EXIF_HEADER) && meta.exif.is_none() => {
                meta.exif = Exif::from_blob(payload[EXIF_HEADER.len()..].to_vec());
            }
            APP1 if payload.starts_with(XMP_HEADER) && meta.xmp.is_none() => {
                meta.xmp = Some(payload[XMP_HEADER.len()..].to_vec());
            }
            APP1 if payload.starts_with(XMP_EXTENSION_HEADER) => {
                meta.jpeg.xmp_extension.push(payload.to_vec());
            }
            APP2 if payload.starts_with(ICC_HEADER) && payload.len() > ICC_HEADER.len() + 2 => {
                icc.push((payload[ICC_HEADER.len()], &payload[ICC_HEADER.len() + 2..]));
            }
            APP13 if payload.starts_with(PHOTOSHOP_HEADER) => photoshop
                .get_or_insert_with(Vec::new)
                .extend_from_slice(&payload[PHOTOSHOP_HEADER.len()..]),
            COM => meta.jpeg.comments.push(payload.to_vec()),
            _ => {}
        }
    }
    icc.sort_by_key(|(sequence, _)| *sequence);
    if !icc.is_empty() {
        meta.icc = Some(
            icc.iter()
                .flat_map(|(_, chunk)| chunk.iter().copied())
                .collect(),
        );
    }
    meta.photoshop = photoshop.and_then(meta::photoshop_without_thumbnails);
    meta.iptc = meta.photoshop.as_deref().and_then(meta::photoshop_iptc);
    meta
}

/// Writes `image` as JPEG (8-bit, alpha dropped), with as much of `meta` as JPEG holds.
pub(crate) fn write<W: Write>(mut out: W, image: &Image, meta: &Metadata) -> Result<()> {
    let (w, h) = image.pixels.dimensions();
    let pixels = if image.gray {
        DynamicImage::ImageLuma8(GrayImage::from_fn(w, h, |x, y| {
            image::Luma([to_u8(gray_value(image.pixels.get_pixel(x, y).0))])
        }))
    } else {
        DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |x, y| {
            image::Rgb(image.pixels.get_pixel(x, y).0.map(to_u8))
        }))
    };
    let mut encoded = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut encoded, QUALITY);
    if let Some(icc) = &meta.icc {
        encoder
            .set_icc_profile(icc.clone())
            .map_err(|e| Error::Image(image::ImageError::Unsupported(e)))?;
    }
    if let Some(density) = meta.density() {
        let (x, y) = density.rounded(1.0);
        let unit = match density.unit {
            DensityUnit::Ratio => PixelDensityUnit::PixelAspectRatio,
            DensityUnit::Inch => PixelDensityUnit::Inches,
            DensityUnit::Centimeter => PixelDensityUnit::Centimeters,
        };
        encoder.set_pixel_density(PixelDensity {
            density: (x.min(65535) as u16, y.min(65535) as u16),
            unit,
        });
    }
    pixels.write_with_encoder(encoder)?;

    // Right after JFIF, before the encoder's ICC profile: EXIF (which readers expect first),
    // XMP, Photoshop resources (with IPTC) and comments.
    let mut segments: Vec<(u8, Vec<u8>)> = Vec::new();
    let resized = image.resized.then_some((w, h));
    if let Some(exif) = exif_segment(meta, resized) {
        segments.push((APP1, exif));
    }
    if let Some(xmp) = &meta.xmp {
        let limit = MAX_SEGMENT - XMP_HEADER.len();
        match meta::xmp_text(xmp).and_then(|text| meta::xmp_within(&text, limit)) {
            Some(text) => {
                segments.push((APP1, [XMP_HEADER, text.as_bytes()].concat()));
                segments.extend(meta.jpeg.xmp_extension.iter().map(|s| (APP1, s.clone())));
            }
            None => eprintln!("XMP left out: too large for JPEG, or not text"),
        }
    }
    let photoshop = meta::photoshop_for_jpeg(meta.photoshop.as_deref(), meta.iptc.as_deref());
    if let Some(block) = photoshop {
        for part in meta::photoshop_parts(&block, MAX_SEGMENT - PHOTOSHOP_HEADER.len()) {
            segments.push((APP13, [PHOTOSHOP_HEADER, part].concat()));
        }
    }
    for comment in &meta.jpeg.comments {
        segments.push((COM, comment.clone()));
    }
    out.write_all(&insert_segments(&encoded, &segments))?;
    Ok(())
}

/// The APP1 payload for `meta`'s EXIF, or `None` if there is none or it is too large even
/// without its maker note (the usual reason for its size). `resized` is the new pixel size of
/// a picture whose size changed.
fn exif_segment(meta: &Metadata, resized: Option<(u32, u32)>) -> Option<Vec<u8>> {
    let exif = meta.exif.as_ref()?;
    let segment = |mut blob: Vec<u8>| {
        if let Some((w, h)) = resized {
            meta::set_pixel_dimensions(&mut blob, w, h);
        }
        (EXIF_HEADER.len() + blob.len() <= MAX_SEGMENT).then(|| [EXIF_HEADER, &blob].concat())
    };
    match exif.to_blob() {
        Ok(blob) => {
            if let Some(segment) = segment(blob) {
                return Some(segment);
            }
        }
        Err(e) => {
            eprintln!("EXIF left out: {e}");
            return None;
        }
    }
    let smaller = exif.to_tags().ok().and_then(|mut tags| {
        for (code, child) in &mut tags.children {
            if *code == EXIF_DIRECTORY {
                child.tags.retain(|(tag, _)| *tag != MAKER_NOTE);
            }
        }
        tiff_io::exif_to_blob(&tags).ok()
    });
    match smaller.and_then(segment) {
        Some(segment) => {
            eprintln!("EXIF maker note left out: too large for JPEG");
            Some(segment)
        }
        None => {
            eprintln!("EXIF left out: over 64 KB, too large for JPEG");
            None
        }
    }
}

/// `encoded` with `extra` inserted right after its JFIF segment (or its start).
fn insert_segments(encoded: &[u8], extra: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let at = match segments(encoded).first() {
        Some(&(APP0, _, end)) => end,
        _ => 2,
    };
    let size: usize = extra.iter().map(|(_, p)| p.len() + 4).sum();
    let mut out = Vec::with_capacity(encoded.len() + size);
    out.extend_from_slice(&encoded[..at]);
    for (marker, payload) in extra {
        out.extend_from_slice(&[0xFF, *marker]);
        out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(payload);
    }
    out.extend_from_slice(&encoded[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_go_right_after_jfif() {
        let segment = |marker: u8, payload: &[u8]| {
            let mut s = vec![0xFF, marker];
            s.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
            s.extend_from_slice(payload);
            s
        };
        let encoded = [
            vec![0xFF, 0xD8],
            segment(APP0, b"JFIF\0\x01\x02\0\0\x01\0\x01\0\0"),
            segment(APP2, b"ICC_PROFILE\0\x01\x01icc"),
            segment(0xDB, b"tables"),
            vec![0xFF, 0xDA, 0, 2, 1, 2, 3, 0xFF, 0xD9],
        ]
        .concat();
        let out = insert_segments(
            &encoded,
            &[(APP1, b"Exif\0\0blob".to_vec()), (COM, b"hello".to_vec())],
        );
        let markers: Vec<u8> = segments(&out).iter().map(|s| s.0).collect();
        assert_eq!(markers, [APP0, APP1, COM, APP2, 0xDB]);
        assert!(out.ends_with(&[0xFF, 0xDA, 0, 2, 1, 2, 3, 0xFF, 0xD9]));
    }
}
