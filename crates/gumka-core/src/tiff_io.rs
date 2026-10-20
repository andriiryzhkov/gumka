// SPDX-License-Identifier: GPL-3.0-or-later

//! TIFF reading and writing that carries metadata across, and the TIFF structure inside EXIF.
//!
//! The output is a modified copy of the input. The main directory's descriptive tags
//! (resolution, orientation, camera, dates, private tags, ...) are copied as they are, except
//! the tags that describe the pixel layout, which the encoder writes itself. The EXIF, GPS and
//! interoperability sub-directories are copied tag by tag and re-linked. Values are converted to
//! the output byte order by type, so big-endian inputs work too; byte streams (ICC profile, XMP,
//! IPTC, Photoshop resources) keep their bytes whatever type they are declared as.
//!
//! JPEG and PNG keep EXIF as a small TIFF file of its own; [`exif_from_blob`] and
//! [`exif_to_blob`] convert between that and tags.
//!
//! Known limit: a maker note with internal absolute offsets may not survive a move. Capture
//! One's TIFF exports do not carry one.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek, Write};
use std::path::Path;

use tiff::decoder::{Decoder, DecodingResult, Limits};
use tiff::encoder::colortype::{self, ColorType as EncoderColorType};
use tiff::encoder::compression::DeflateLevel;
use tiff::encoder::{Compression, DirectoryEncoder, TiffEncoder, TiffKindStandard, TiffValue};
use tiff::tags::{ByteOrder, ExtraSamples, IfdPointer, Predictor, Tag, Type, ValueBuffer};
use tiff::{ColorType, Directory};

use crate::Rgb16;
use crate::meta::{self, Density, DensityUnit, Exif, Metadata};
use crate::picture::{Decoded, Error, Image, Result, gray_value, to_u8};

const ICC_PROFILE: u16 = 34675;
const XMP: u16 = 700;
const IPTC: u16 = 33723;
const PHOTOSHOP: u16 = 34377;
const RESOLUTION_UNIT: u16 = 296;
const MATTEING: u16 = 32995;

/// Byte streams, whatever type they are declared as: kept in the file's byte order and given
/// homes of their own in [`Metadata`].
const BLOB_TAGS: [u16; 4] = [ICC_PROFILE, XMP, IPTC, PHOTOSHOP];

/// Sub-directory pointers that are followed and rewritten: EXIF, GPS, interoperability.
const SUB_DIRECTORIES: [u16; 3] = [34665, 34853, 40965];

/// How deep sub-directories are followed (main, EXIF, interoperability, and one spare).
const MAX_DEPTH: usize = 3;

/// Main-directory tags that describe the pixel data, or point at data that is not copied.
const LAYOUT_TAGS: &[u16] = &[
    254, 255, // new subfile type, subfile type
    256, 257, 258, 259, 262, // width, length, bits per sample, compression, photometric
    273, 277, 278, 279, // strip offsets, samples per pixel, rows per strip, strip byte counts
    284, 288, 289, // planar configuration, free offsets, free byte counts
    317, 322, 323, 324, 325, // predictor, tile width / length / offsets / byte counts
    330, 338, 339, 340, 341, // sub-IFDs, extra samples, sample format, min / max sample value
    347, 513, 514, // JPEG tables, JPEG thumbnail offset / length
    529, 530, 531,   // YCbCr coefficients, subsampling, positioning
    32995, // matteing: the obsolete name of extra samples
    37724, // Photoshop layer data: would bring back the old pixels
];

/// A tag's value as raw bytes in the native byte order, which the encoder writes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TagValue {
    pub ty: Type,
    pub bytes: Vec<u8>,
}

impl TagValue {
    pub fn from_short(value: u16) -> Self {
        TagValue {
            ty: Type::SHORT,
            bytes: value.to_ne_bytes().to_vec(),
        }
    }

    /// A RATIONAL close to `value`.
    pub fn from_rational(value: f64) -> Self {
        let (n, d) = if value.fract() == 0.0 {
            (value, 1)
        } else {
            ((value * 10_000.0).round(), 10_000)
        };
        let n = n.clamp(0.0, f64::from(u32::MAX)) as u32;
        TagValue {
            ty: Type::RATIONAL,
            bytes: [n.to_ne_bytes(), u32::to_ne_bytes(d)].concat(),
        }
    }

    pub fn short(&self) -> Option<u16> {
        let b = self.bytes.get(0..2).filter(|_| self.ty == Type::SHORT)?;
        Some(u16::from_ne_bytes([b[0], b[1]]))
    }

    pub fn rational(&self) -> Option<f64> {
        let b = self.bytes.get(0..8).filter(|_| self.ty == Type::RATIONAL)?;
        let n = u32::from_ne_bytes([b[0], b[1], b[2], b[3]]);
        let d = u32::from_ne_bytes([b[4], b[5], b[6], b[7]]);
        (d != 0).then(|| f64::from(n) / f64::from(d))
    }
}

/// A directory's tags, and the sub-directories it points to.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TagDirectory {
    pub tags: Vec<(u16, TagValue)>,
    pub children: Vec<(u16, TagDirectory)>,
}

impl TagDirectory {
    pub fn get(&self, code: u16) -> Option<&TagValue> {
        self.tags.iter().find(|(c, _)| *c == code).map(|(_, v)| v)
    }

    fn set(&mut self, code: u16, value: TagValue) {
        match self.tags.iter_mut().find(|(c, _)| *c == code) {
            Some((_, v)) => *v = value,
            None => self.tags.push((code, value)),
        }
    }

    fn is_empty(&self) -> bool {
        self.tags.is_empty() && self.children.is_empty()
    }
}

/// What a TIFF source keeps for being written as TIFF again: its compression and predictor,
/// and the types its byte streams were declared as.
pub(crate) struct TiffSource {
    compression: u16,
    predictor: u16,
    blob_types: Vec<(u16, Type)>,
}

pub(crate) fn read(path: &Path) -> Result<Decoded> {
    let file = File::open(path)?;
    // A tag's value is in the file, so one that claims to be larger fails before anything is
    // allocated for it. Only the decoded picture may be larger, while it is read.
    let size = usize::try_from(file.metadata()?.len()).unwrap_or(usize::MAX);
    let mut limits = Limits::unlimited();
    limits.decoding_buffer_size = size;
    limits.intermediate_buffer_size = size;
    let mut decoder = Decoder::new(BufReader::new(file))?.with_limits(limits.clone());
    let (width, height) = decoder.dimensions()?;
    // Color samples (RGB, or gray), then extra ones, checked below.
    let (color, bit_depth) = match decoder.colortype()? {
        ColorType::RGB(b @ (8 | 16)) | ColorType::RGBA(b @ (8 | 16)) => (3, b),
        ColorType::Gray(b @ (8 | 16)) => (1, b),
        ColorType::Multiband {
            bit_depth: b @ (8 | 16),
            num_samples: 2,
        } => (1, b),
        other => {
            return Err(Error::Unsupported(format!(
                "unsupported TIFF color type {other:?}: Gumka opens 8- or 16-bit RGB or gray"
            )));
        }
    };
    // At most one sample more, which must be alpha: another channel would be lost on saving.
    let samples = usize::from(
        decoder
            .find_tag_unsigned::<u16>(Tag::SamplesPerPixel)?
            .unwrap_or(1),
    );
    let extra = decoder
        .find_tag_unsigned_vec::<u16>(Tag::ExtraSamples)?
        .unwrap_or_default();
    // Matteing, ExtraSamples' obsolete predecessor, says the alpha is associated.
    let matteing = decoder
        .find_tag_unsigned::<u16>(Tag::from_u16_exhaustive(MATTEING))
        .ok()
        .flatten()
        == Some(1);
    let alpha_kind = match (samples.checked_sub(color), extra.as_slice()) {
        (Some(0), []) => None,
        (Some(1), []) if matteing => Some(ExtraSamples::AssociatedAlpha),
        (Some(1), [] | [2]) => Some(ExtraSamples::UnassociatedAlpha),
        (Some(1), [1]) => Some(ExtraSamples::AssociatedAlpha),
        _ => {
            return Err(Error::Unsupported(
                "this TIFF has an extra channel that isn't transparency; Gumka can't keep it, \
                 so it doesn't open such files"
                    .into(),
            ));
        }
    };
    let white_is_zero =
        decoder.find_tag_unsigned::<u16>(Tag::PhotometricInterpretation)? == Some(0);
    if white_is_zero && samples > 1 {
        return Err(Error::Unsupported(
            "unsupported TIFF: white-is-zero gray with transparency".into(),
        ));
    }
    // tiff reads only the first plane of a picture stored channel by channel.
    if samples > 1 && decoder.find_tag_unsigned::<u16>(Tag::PlanarConfiguration)? == Some(2) {
        return Err(Error::Unsupported(
            "planar TIFFs (channels stored one after another) aren't supported".into(),
        ));
    }
    let channels = samples;
    let mut picture_limits = limits.clone();
    picture_limits.decoding_buffer_size = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(channels * usize::from(bit_depth / 8))
        .max(size);
    decoder = decoder.with_limits(picture_limits);
    let decoded = decoder.read_image();
    decoder = decoder.with_limits(limits);
    let values: Vec<u16> = match decoded? {
        DecodingResult::U8(v) => v.into_iter().map(|s| u16::from(s) * 257).collect(),
        DecodingResult::U16(v) => v,
        _ => return Err(Error::Unsupported("unsupported TIFF sample format".into())),
    };
    let count = width as usize * height as usize;
    if values.len() < count * channels {
        return Err(Error::Unsupported(
            "TIFF image data is shorter than expected".into(),
        ));
    }
    let values = &values[..count * channels];
    let pixels: Vec<u16> = values
        .chunks_exact(channels)
        .flat_map(|p| {
            if color == 1 {
                [p[0]; 3]
            } else {
                [p[0], p[1], p[2]]
            }
        })
        .collect();
    let alpha = alpha_kind.map(|_| values.chunks_exact(channels).map(|p| p[color]).collect());
    let pixels = Rgb16::from_raw(width, height, pixels)
        .ok_or_else(|| Error::Unsupported("TIFF image data size mismatch".into()))?;

    let compression = decoder.find_tag_unsigned(Tag::Compression)?.unwrap_or(1);
    let predictor = decoder.find_tag_unsigned(Tag::Predictor)?.unwrap_or(1);
    let pointer = decoder
        .ifd_pointer()
        .ok_or_else(|| Error::Unsupported("TIFF has no image directory".into()))?;
    let directory = decoder.read_directory(pointer)?;
    let mut blobs = Vec::new();
    let mut visited = HashSet::from([pointer.0]);
    let tags = read_directory(&mut decoder, &directory, 0, &mut visited, Some(&mut blobs));

    let blob = |code| {
        blobs
            .iter()
            .find(|(c, _)| *c == code)
            .map(|(_, v): &(u16, TagValue)| v.bytes.clone())
    };
    let photoshop = blob(PHOTOSHOP).and_then(meta::photoshop_without_thumbnails);
    let iptc = blob(IPTC)
        .map(|iptc| meta::iptc_records(&iptc).to_vec())
        .or_else(|| photoshop.as_deref().and_then(meta::photoshop_iptc));
    let meta = Metadata {
        icc: blob(ICC_PROFILE),
        exif: (!tags.is_empty()).then_some(Exif::Tags(tags)),
        xmp: blob(XMP),
        iptc,
        photoshop,
        ..Default::default()
    };
    Ok(Decoded {
        pixels,
        alpha,
        associated_alpha: alpha_kind == Some(ExtraSamples::AssociatedAlpha),
        bit_depth,
        gray: color == 1,
        meta,
        tiff: Some(TiffSource {
            compression,
            predictor,
            blob_types: blobs.iter().map(|(c, v)| (*c, v.ty)).collect(),
        }),
    })
}

/// Reads a directory's tags and, through their pointers, its EXIF, GPS and interoperability
/// directories. A tag or sub-directory that can't be read is left out, and so is a pointer
/// back to a directory already read or one nested too deep. With `blobs`, this is a main
/// directory: layout tags are skipped, and byte streams go to `blobs` in the file's byte order.
fn read_directory<R: Read + Seek>(
    decoder: &mut Decoder<R>,
    directory: &Directory,
    depth: usize,
    visited: &mut HashSet<u64>,
    mut blobs: Option<&mut Vec<(u16, TagValue)>>,
) -> TagDirectory {
    let main = blobs.is_some();
    let mut out = TagDirectory::default();
    for (tag, entry) in directory.iter() {
        let code = tag.to_u16();
        if main && LAYOUT_TAGS.contains(&code) {
            continue;
        }
        if SUB_DIRECTORIES.contains(&code) {
            let child = decoder
                .read_directory_tags(directory)
                .find_tag(tag)
                .ok()
                .flatten()
                .and_then(|v| v.into_ifd_pointer().ok())
                .filter(|pointer| depth < MAX_DEPTH && visited.insert(pointer.0))
                .and_then(|pointer| decoder.read_directory(pointer).ok());
            if let Some(child) = child {
                let child = read_directory(decoder, &child, depth + 1, visited, None);
                out.children.push((code, child));
            }
            continue;
        }
        if matches!(entry.field_type(), Type::IFD | Type::IFD8) {
            continue; // other pointers cannot be moved safely
        }
        if matches!(entry.field_type(), Type::LONG8 | Type::SLONG8) {
            continue; // BigTIFF only: a classic TIFF or EXIF can't hold them
        }
        let mut value = ValueBuffer::empty(entry.field_type());
        if decoder
            .read_directory_tags(directory)
            .find_tag_buf(tag, &mut value)
            .is_err()
        {
            continue;
        }
        match blobs.as_deref_mut() {
            Some(blobs) if BLOB_TAGS.contains(&code) => blobs.push((code, raw(&value))),
            _ => {
                value.set_byte_order(ByteOrder::native());
                out.tags.push((code, raw(&value)));
            }
        }
    }
    out
}

fn raw(value: &ValueBuffer) -> TagValue {
    TagValue {
        ty: value.data_type(),
        bytes: value.as_bytes().to_vec(),
    }
}

/// The tags of an EXIF blob: a TIFF structure, as JPEG and PNG keep EXIF.
pub(crate) fn exif_from_blob(blob: &[u8]) -> Result<TagDirectory> {
    // tiff's decoder only opens a file whose first directory is an image, which an EXIF
    // blob's isn't. So a 1 x 1 image directory is appended and the header pointed at it; the
    // blob's own offsets count from its start and stay valid.
    let le = match blob.get(0..4) {
        Some(b"II*\0") => true,
        Some(b"MM\0*") => false,
        _ => return Err(Error::Unsupported("EXIF isn't a TIFF structure".into())),
    };
    let u16b = |v: u16| if le { v.to_le_bytes() } else { v.to_be_bytes() };
    let u32b = |v: u32| if le { v.to_le_bytes() } else { v.to_be_bytes() };
    let header: [u8; 4] = blob
        .get(4..8)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| Error::Unsupported("EXIF is cut short".into()))?;
    let ifd0 = if le {
        u32::from_le_bytes(header)
    } else {
        u32::from_be_bytes(header)
    };
    let too_big = || Error::Unsupported("EXIF is too large".into());
    let mut data = blob.to_vec();
    if data.len() % 2 == 1 {
        data.push(0);
    }
    let pixel = u32::try_from(data.len()).map_err(|_| too_big())?;
    data.extend_from_slice(&[0, 0]);
    let image = u32::try_from(data.len()).map_err(|_| too_big())?;
    let entries: [(u16, u16, u32); 7] = [
        (256, 3, 1),     // width
        (257, 3, 1),     // length
        (258, 3, 8),     // bits per sample
        (262, 3, 1),     // photometric: black is zero
        (273, 4, pixel), // strip offset
        (278, 3, 1),     // rows per strip
        (279, 4, 1),     // strip byte count
    ];
    data.extend_from_slice(&u16b(entries.len() as u16));
    for (tag, ty, value) in entries {
        data.extend_from_slice(&u16b(tag));
        data.extend_from_slice(&u16b(ty));
        data.extend_from_slice(&u32b(1));
        if ty == 3 {
            data.extend_from_slice(&u16b(value as u16));
            data.extend_from_slice(&[0, 0]);
        } else {
            data.extend_from_slice(&u32b(value));
        }
    }
    data.extend_from_slice(&u32b(0));
    data[4..8].copy_from_slice(&u32b(image));

    // No value in the blob is larger than the blob: one that claims more fails before
    // anything is allocated for it.
    let mut limits = Limits::unlimited();
    limits.decoding_buffer_size = data.len();
    limits.intermediate_buffer_size = data.len();
    let mut decoder = Decoder::new(Cursor::new(data))?.with_limits(limits);
    let pointer = IfdPointer(u64::from(ifd0));
    let directory = decoder.read_directory(pointer)?;
    let mut visited = HashSet::from([pointer.0, u64::from(image)]);
    // Byte streams in the main directory have homes of their own in JPEG and PNG.
    let mut blobs = Vec::new();
    Ok(read_directory(
        &mut decoder,
        &directory,
        0,
        &mut visited,
        Some(&mut blobs),
    ))
}

/// An EXIF blob holding `tags`: a little TIFF structure with no image and no thumbnail.
pub(crate) fn exif_to_blob(tags: &TagDirectory) -> Result<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    {
        let mut encoder = TiffEncoder::new(&mut out)?;
        let children = write_children(&mut encoder, &tags.children)?;
        let mut ifd0 = encoder.image_directory()?;
        for (code, value) in &tags.tags {
            put(&mut ifd0, *code, value.ty, &value.bytes)?;
        }
        for &(code, offset) in &children {
            ifd0.write_tag(Tag::from_u16_exhaustive(code), offset)?;
        }
        ifd0.finish()?;
    }
    Ok(out.into_inner())
}

/// Writes `image` as TIFF, with `meta`. `source` is set when the picture came from a TIFF,
/// whose compression is kept where it can be written; LZW otherwise, which every reader takes.
pub(crate) fn write<W: Write + Seek>(
    out: W,
    image: &Image,
    source: Option<&TiffSource>,
    meta: &Metadata,
) -> Result<()> {
    let compression = match source.map(|s| s.compression) {
        Some(1) => Compression::Uncompressed,
        Some(5) => Compression::Lzw,
        Some(32773) => Compression::Packbits,
        Some(8 | 32946) => Compression::Deflate(DeflateLevel::Balanced),
        _ => Compression::Lzw,
    };
    let gray_alpha = image.gray && image.alpha.is_some();
    let predictor = match compression {
        Compression::Uncompressed | Compression::Packbits => Predictor::None,
        // tiff's horizontal predictor knows only the color samples, not an extra alpha.
        _ if gray_alpha => Predictor::None,
        _ if source.is_some_and(|s| s.predictor == 1) => Predictor::None,
        _ => Predictor::Horizontal,
    };
    let mut encoder = TiffEncoder::new(out)?
        .with_compression(compression)
        .with_predictor(predictor);

    let mut tags = match meta.exif.as_ref().map(Exif::to_tags) {
        Some(Ok(tags)) => tags,
        Some(Err(e)) => {
            eprintln!("EXIF left out: {e}");
            TagDirectory::default()
        }
        None => TagDirectory::default(),
    };
    // The encoder writes a resolution of 1/1 with no unit, which macOS reads as 1 pixel per
    // inch. A resolution that only says the pixels are square (no unit, equal values: JFIF's
    // and pHYs's usual, and the tiff crate's default) says no more than none. Without one, 72
    // pixels per inch, which readers assume when a file gives none; without a unit, inches,
    // TIFF's default.
    let square = |d: &Density| d.unit == DensityUnit::Ratio && d.x == d.y;
    match Density::from_tags(&tags) {
        Some(own) if !square(&own) => {
            if tags.get(RESOLUTION_UNIT).is_none() {
                tags.set(RESOLUTION_UNIT, TagValue::from_short(2));
            }
        }
        _ => {
            let density = meta.density.filter(|d| !square(d)).unwrap_or(Density {
                x: 72.0,
                y: 72.0,
                unit: DensityUnit::Inch,
            });
            for (code, value) in density.to_tags() {
                tags.set(code, value);
            }
        }
    }
    let children = write_children(&mut encoder, &tags.children)?;
    let type_of = |code: u16, default: Type| {
        source
            .and_then(|s| s.blob_types.iter().find(|(c, _)| *c == code))
            .map_or(default, |(_, ty)| *ty)
    };
    let mut blobs: Vec<(u16, Type, Vec<u8>)> = Vec::new();
    if let Some(icc) = &meta.icc {
        blobs.push((ICC_PROFILE, Type::UNDEFINED, icc.clone()));
    }
    if let Some(xmp) = &meta.xmp {
        blobs.push((XMP, type_of(XMP, Type::BYTE), xmp.clone()));
    }
    // IPTC has its own tag; a copy in the block from another format would make readers find
    // the records twice. A TIFF's block stays as it was: Photoshop keeps IPTC in both.
    let photoshop = match (&meta.photoshop, &meta.iptc) {
        (Some(block), Some(_)) if source.is_none() => meta::photoshop_without_iptc(block),
        (block, _) => block.clone(),
    };
    if let Some(iptc) = &meta.iptc {
        // Photoshop declares IPTC as LONG, padded with zeros, as exiftool expects too.
        let ty = type_of(IPTC, Type::LONG);
        let mut iptc = iptc.clone();
        iptc.resize(iptc.len().next_multiple_of(type_size(ty)), 0);
        blobs.push((IPTC, ty, iptc));
    }
    if let Some(photoshop) = photoshop {
        blobs.push((PHOTOSHOP, type_of(PHOTOSHOP, Type::BYTE), photoshop));
    }
    let extra = Extra {
        tags: &tags,
        children: &children,
        blobs: &blobs,
        alpha: image.alpha.map(|_| {
            if image.associated_alpha {
                ExtraSamples::AssociatedAlpha
            } else {
                ExtraSamples::UnassociatedAlpha
            }
        }),
    };

    let (w, h) = image.pixels.dimensions();
    let rgb = image.pixels.as_raw();
    let gray = || rgb.as_chunks::<3>().0.iter().map(|&p| gray_value(p));
    let e = &mut encoder;
    match (image.gray, image.bit_depth, image.alpha) {
        (false, 16, None) => write_image::<colortype::RGB16, _>(e, w, h, rgb, &extra),
        (false, 16, Some(alpha)) => {
            let data = interleave(rgb.as_chunks::<3>().0, alpha, |v| v);
            write_image::<colortype::RGBA16, _>(e, w, h, &data, &extra)
        }
        (false, _, None) => {
            let data: Vec<u8> = rgb.iter().map(|&v| to_u8(v)).collect();
            write_image::<colortype::RGB8, _>(e, w, h, &data, &extra)
        }
        (false, _, Some(alpha)) => {
            let data = interleave(rgb.as_chunks::<3>().0, alpha, to_u8);
            write_image::<colortype::RGBA8, _>(e, w, h, &data, &extra)
        }
        (true, 16, None) => {
            let data: Vec<u16> = gray().collect();
            write_image::<colortype::Gray16, _>(e, w, h, &data, &extra)
        }
        (true, 16, Some(alpha)) => {
            let data: Vec<u16> = gray().zip(alpha).flat_map(|(g, &a)| [g, a]).collect();
            write_image::<colortype::Gray16, _>(e, w, h, &data, &extra)
        }
        (true, _, None) => {
            let data: Vec<u8> = gray().map(to_u8).collect();
            write_image::<colortype::Gray8, _>(e, w, h, &data, &extra)
        }
        (true, _, Some(alpha)) => {
            let data: Vec<u8> = gray()
                .zip(alpha)
                .flat_map(|(g, &a)| [to_u8(g), to_u8(a)])
                .collect();
            write_image::<colortype::Gray8, _>(e, w, h, &data, &extra)
        }
    }
}

struct Extra<'a> {
    tags: &'a TagDirectory,
    children: &'a [(u16, u32)],
    /// Byte streams: tag, declared type, bytes.
    blobs: &'a [(u16, Type, Vec<u8>)],
    alpha: Option<ExtraSamples>,
}

fn write_image<C, W>(
    encoder: &mut TiffEncoder<W>,
    width: u32,
    height: u32,
    data: &[C::Inner],
    extra: &Extra,
) -> Result<()>
where
    C: EncoderColorType,
    [C::Inner]: TiffValue,
    W: Write + Seek,
{
    let mut image = encoder.new_image::<C>(width, height)?;
    // Gray's alpha is an extra sample to the encoder, which then counts it in; RGBA's is part
    // of the color type, and only its kind is written below.
    let gray_alpha = C::BITS_PER_SAMPLE.len() == 1;
    if gray_alpha && let Some(alpha) = extra.alpha {
        image.extra_samples(&[alpha])?;
    }
    let dir = image.encoder();
    // These replace what the encoder wrote, the resolution among them (see `write`).
    for (code, value) in &extra.tags.tags {
        put(dir, *code, value.ty, &value.bytes)?;
    }
    for &(code, offset) in extra.children {
        dir.write_tag(Tag::from_u16_exhaustive(code), offset)?;
    }
    for (code, ty, bytes) in extra.blobs {
        // A declared type the bytes don't fill evenly can't be written; UNDEFINED always can.
        let ty = if bytes.len() % type_size(*ty) == 0 {
            *ty
        } else {
            Type::UNDEFINED
        };
        put(dir, *code, ty, bytes)?;
    }
    if !gray_alpha && let Some(alpha) = extra.alpha {
        dir.write_tag(Tag::ExtraSamples, alpha.to_u16())?;
    }
    image.write_data(data)?;
    Ok(())
}

/// Writes one tag from raw bytes, already in the file's byte order. Values stored apart from
/// the directory are padded to an even length, so the next one starts on a word boundary.
fn put<W: Write + Seek>(
    dir: &mut DirectoryEncoder<'_, W, TiffKindStandard>,
    code: u16,
    ty: Type,
    bytes: &[u8],
) -> Result<()> {
    let entry = dir.write_entry_bytes(ty, bytes)?;
    if bytes.len() > 4 && bytes.len() % 2 == 1 {
        dir.write_data(&[0u8][..])?;
    }
    dir.extend_from(&Directory::from_iter([(
        Tag::from_u16_exhaustive(code),
        entry,
    )]));
    Ok(())
}

fn type_size(ty: Type) -> usize {
    match ty {
        Type::SHORT | Type::SSHORT => 2,
        Type::LONG | Type::SLONG | Type::FLOAT | Type::IFD => 4,
        Type::RATIONAL | Type::SRATIONAL | Type::DOUBLE | Type::LONG8 | Type::SLONG8 => 8,
        Type::IFD8 => 8,
        _ => 1,
    }
}

/// Writes sub-directories (and theirs, first) and returns `(pointer tag, offset)` pairs.
fn write_children<W: Write + Seek>(
    encoder: &mut TiffEncoder<W>,
    children: &[(u16, TagDirectory)],
) -> Result<Vec<(u16, u32)>> {
    let mut written = Vec::new();
    for (code, child) in children {
        if child.is_empty() {
            continue;
        }
        let grandchildren = write_children(encoder, &child.children)?;
        let mut dir = encoder.extra_directory()?;
        for (tag, value) in &child.tags {
            put(&mut dir, *tag, value.ty, &value.bytes)?;
        }
        for &(tag, offset) in &grandchildren {
            dir.write_tag(Tag::from_u16_exhaustive(tag), offset)?;
        }
        written.push((*code, dir.finish_with_offsets()?.offset));
    }
    Ok(written)
}

fn interleave<T: Copy>(rgb: &[[u16; 3]], alpha: &[u16], convert: impl Fn(u16) -> T) -> Vec<T> {
    rgb.iter()
        .zip(alpha)
        .flat_map(|(p, &a)| [p[0], p[1], p[2], a])
        .map(convert)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::io::BufWriter;

    use super::*;
    use crate::picture::Picture;

    const XMP: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'>gumka test</x:xmpmeta>";
    const IPTC: &[u8] = &[0x1c, 0x02, 0x05, 0x00, 0x04, b't', b'e', b's', b't'];

    fn icc() -> Vec<u8> {
        let mut icc: Vec<u8> = (0..=255u8).cycle().take(560).collect();
        icc[16..20].copy_from_slice(b"RGB ");
        icc
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gumka-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn pixels(w: u32, h: u32) -> Rgb16 {
        Rgb16::from_fn(w, h, |x, y| {
            image::Rgb([(x * 977 + y) as u16, (y * 499) as u16, (x ^ y) as u16 * 13])
        })
    }

    /// Writes a 16-bit TIFF with ICC, XMP, IPTC, resolution, orientation and an EXIF
    /// directory, in the given byte order's native encoder (the crate writes native order).
    fn write_source(path: &Path, image: &Rgb16) {
        let mut encoder = TiffEncoder::new(BufWriter::new(File::create(path).unwrap()))
            .unwrap()
            .with_compression(Compression::Lzw)
            .with_predictor(Predictor::Horizontal);
        let mut exif = encoder.extra_directory().unwrap();
        exif.write_tag(Tag::Unknown(36867), "2026:10:19 12:00:00")
            .unwrap(); // DateTimeOriginal
        exif.write_tag(
            Tag::Unknown(33434),
            tiff::encoder::Rational { n: 1, d: 250 },
        )
        .unwrap(); // ExposureTime
        exif.write_tag(Tag::Unknown(34855), 400u16).unwrap(); // ISO
        let exif = exif.finish_with_offsets().unwrap().offset;

        let mut tiff = encoder
            .new_image::<colortype::RGB16>(image.width(), image.height())
            .unwrap();
        let dir = tiff.encoder();
        let icc_entry = dir.write_entry_bytes(Type::UNDEFINED, &icc()).unwrap();
        dir.extend_from(&Directory::from_iter([(Tag::IccProfile, icc_entry)]));
        dir.write_tag(Tag::Unknown(700), XMP).unwrap();
        let iptc = dir.write_entry_bytes(Type::UNDEFINED, IPTC).unwrap();
        dir.extend_from(&Directory::from_iter([(Tag::Unknown(33723), iptc)]));
        dir.write_tag(Tag::Orientation, 6u16).unwrap();
        dir.write_tag(Tag::Software, "Capture One").unwrap();
        dir.write_tag(Tag::Unknown(34665), exif).unwrap();
        tiff.resolution(
            tiff::tags::ResolutionUnit::Inch,
            tiff::encoder::Rational { n: 300, d: 1 },
        );
        tiff.write_data(image.as_raw()).unwrap();
    }

    fn raw_tag(decoder: &mut Decoder<BufReader<File>>, code: u16) -> Option<(Type, Vec<u8>)> {
        let tag = Tag::from_u16_exhaustive(code);
        let mut value = ValueBuffer::empty(Type::BYTE);
        let entry = decoder.image_ifd().find_tag_buf(tag, &mut value).unwrap()?;
        value.set_byte_order(ByteOrder::native());
        Some((entry.field_type(), value.as_bytes().to_vec()))
    }

    #[test]
    fn metadata_and_pixels_survive_a_round_trip() {
        let dir = test_dir("tiff-meta");
        let (src, out) = (dir.join("in.tif"), dir.join("out.tif"));
        let image = pixels(37, 23);
        write_source(&src, &image);

        let picture = Picture::open(&src).unwrap();
        assert_eq!(picture.bit_depth(), 16);
        assert_eq!(picture.orientation(), 6);
        assert_eq!(
            picture.display_size(),
            (23, 37),
            "rotated 90°: width and height swap"
        );
        assert!(picture.has_icc_profile());
        assert_eq!(picture.pixels, image);
        picture.save(&out).unwrap();

        let mut a = Decoder::new(BufReader::new(File::open(&src).unwrap())).unwrap();
        let mut b = Decoder::new(BufReader::new(File::open(&out).unwrap())).unwrap();
        for code in [ICC_PROFILE, 700, 33723, 274, 305, 282, 283, 296] {
            let (ta, va) =
                raw_tag(&mut a, code).unwrap_or_else(|| panic!("source lacks tag {code}"));
            let (tb, vb) =
                raw_tag(&mut b, code).unwrap_or_else(|| panic!("output lost tag {code}"));
            assert_eq!((ta, &va), (tb, &vb), "tag {code} changed");
        }
        assert_eq!(
            b.find_tag_unsigned::<u16>(Tag::Compression).unwrap(),
            Some(5),
            "LZW kept"
        );

        // EXIF directory: same tags and values, re-linked.
        let exif_b = b
            .get_tag(Tag::Unknown(34665))
            .unwrap()
            .into_ifd_pointer()
            .unwrap();
        let exif_b = b.read_directory(exif_b).unwrap();
        let mut tags = b.read_directory_tags(&exif_b);
        assert_eq!(
            tags.find_tag(Tag::Unknown(34855))
                .unwrap()
                .unwrap()
                .into_u16()
                .unwrap(),
            400
        );
        assert_eq!(
            tags.find_tag(Tag::Unknown(36867))
                .unwrap()
                .unwrap()
                .into_string()
                .unwrap(),
            "2026:10:19 12:00:00"
        );

        assert_eq!(Picture::open(&out).unwrap().pixels, image);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn eight_bit_and_alpha_round_trip() {
        let dir = test_dir("tiff-8bit");
        let path = dir.join("in.tif");
        let rgb: Vec<u8> = (0..11 * 7 * 4).map(|i| (i * 37 % 256) as u8).collect();
        TiffEncoder::new(BufWriter::new(File::create(&path).unwrap()))
            .unwrap()
            .write_image::<colortype::RGBA8>(11, 7, &rgb)
            .unwrap();

        let picture = Picture::open(&path).unwrap();
        assert_eq!(picture.bit_depth(), 8);
        let out = dir.join("out.tif");
        picture.save(&out).unwrap();
        let mut decoder = Decoder::new(BufReader::new(File::open(&out).unwrap())).unwrap();
        assert_eq!(decoder.colortype().unwrap(), ColorType::RGBA(8));
        match decoder.read_image().unwrap() {
            DecodingResult::U8(data) => assert_eq!(data, rgb),
            _ => panic!("expected 8-bit data"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
