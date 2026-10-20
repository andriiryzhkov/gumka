// SPDX-License-Identifier: GPL-3.0-or-later

//! Metadata carried from the opened file to every save, whatever the two formats are.
//!
//! One record holds what any of the formats can carry: the ICC profile, EXIF (with GPS), XMP,
//! IPTC, Photoshop's image resources and the resolution, plus what only PNG or only JPEG can
//! hold (text chunks, comments). Each writer puts as much of it as its format holds where that
//! format keeps it.
//!
//! Thumbnails are dropped on the way in, so that no preview of a saved file shows the erased
//! object: EXIF's second directory, and Photoshop's thumbnail resources.

use std::fmt::Write as _;

use crate::jpeg_io::JpegExtras;
use crate::picture::{Error, Result};
use crate::png_io::PngExtras;
use crate::tiff_io::{self, TagDirectory, TagValue};

#[derive(Default)]
pub(crate) struct Metadata {
    pub icc: Option<Vec<u8>>,
    pub exif: Option<Exif>,
    pub xmp: Option<Vec<u8>>,
    /// IPTC-IIM records, as in TIFF tag 33723 or Photoshop resource 0x0404.
    pub iptc: Option<Vec<u8>>,
    /// Photoshop image resources (TIFF tag 34377, JPEG APP13), without thumbnails.
    pub photoshop: Option<Vec<u8>>,
    /// From a PNG pHYs chunk or a JPEG JFIF header.
    pub density: Option<Density>,
    pub png: PngExtras,
    pub jpeg: JpegExtras,
}

impl Metadata {
    /// EXIF orientation 1-8, 1 when there is none.
    pub fn orientation(&self) -> u8 {
        self.exif.as_ref().map_or(1, Exif::orientation)
    }

    /// The resolution: EXIF's (or the TIFF's) resolution tags, else the PNG or JFIF density.
    pub fn density(&self) -> Option<Density> {
        self.exif
            .as_ref()
            .and_then(|exif| Density::from_tags(&exif.to_tags().ok()?))
            .or(self.density)
    }
}

/// EXIF, with its GPS and interoperability directories.
pub(crate) enum Exif {
    /// As JPEG and PNG store it: a small TIFF file, kept byte for byte so that a maker note's
    /// internal offsets stay valid. Its thumbnail is unlinked and blanked on the way in.
    Blob(Vec<u8>),
    /// As a TIFF stores it: the main directory's descriptive tags, with the EXIF, GPS and
    /// interoperability directories as children.
    Tags(TagDirectory),
}

const ORIENTATION: u16 = 274;

impl Exif {
    /// EXIF from JPEG or PNG, without its thumbnail; `None` (with a log line) if it isn't a
    /// TIFF structure that reads, which no save path should copy.
    pub fn from_blob(mut blob: Vec<u8>) -> Option<Self> {
        drop_thumbnail(&mut blob);
        match tiff_io::exif_from_blob(&blob) {
            Ok(_) => Some(Exif::Blob(blob)),
            Err(e) => {
                eprintln!("EXIF left out: {e}");
                None
            }
        }
    }

    pub fn orientation(&self) -> u8 {
        let value = match self {
            Exif::Blob(blob) => {
                image::metadata::Orientation::from_exif_chunk(blob).map(|o| u16::from(o.to_exif()))
            }
            Exif::Tags(tags) => tags.get(ORIENTATION).and_then(TagValue::short),
        };
        match value {
            Some(o @ 1..=8) => o as u8,
            _ => 1,
        }
    }

    /// As tags, for a TIFF. A maker note is moved, which can break its internal offsets.
    pub fn to_tags(&self) -> Result<TagDirectory> {
        match self {
            Exif::Blob(blob) => tiff_io::exif_from_blob(blob),
            Exif::Tags(tags) => Ok(tags.clone()),
        }
    }

    /// As a blob, for JPEG and PNG.
    pub fn to_blob(&self) -> Result<Vec<u8>> {
        match self {
            Exif::Blob(blob) => Ok(blob.clone()),
            Exif::Tags(tags) => tiff_io::exif_to_blob(tags),
        }
    }
}

/// Unlinks EXIF's second directory (the thumbnail's) and blanks the thumbnail it points at, in
/// place, so every other offset in the blob stays valid. Leaves a malformed blob alone.
fn drop_thumbnail(blob: &mut [u8]) {
    let le = match blob.get(0..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return,
    };
    let u16_at = |b: &[u8], at: usize| {
        let s: [u8; 2] = b.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if le {
            u16::from_le_bytes(s)
        } else {
            u16::from_be_bytes(s)
        })
    };
    let u32_at = |b: &[u8], at: usize| {
        let s: [u8; 4] = b.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if le {
            u32::from_le_bytes(s)
        } else {
            u32::from_be_bytes(s)
        } as usize)
    };
    let Some(ifd0) = u32_at(blob, 4) else { return };
    let Some(count) = u16_at(blob, ifd0) else {
        return;
    };
    let next_at = ifd0 + 2 + 12 * usize::from(count);
    let Some(ifd1) = u32_at(blob, next_at).filter(|&o| o != 0) else {
        return;
    };
    blob[next_at..next_at + 4].fill(0);

    // The thumbnail: JPEG (513 offset, 514 length) or one strip (273 offset, 279 length).
    let Some(entries) = u16_at(blob, ifd1).map(usize::from) else {
        return;
    };
    let (mut offset, mut length) = (None, None);
    for i in 0..entries {
        let at = ifd1 + 2 + 12 * i;
        let (Some(tag), Some(ty), Some(1)) =
            (u16_at(blob, at), u16_at(blob, at + 2), u32_at(blob, at + 4))
        else {
            continue;
        };
        let value = match ty {
            3 => u16_at(blob, at + 8).map(usize::from),
            4 => u32_at(blob, at + 8),
            _ => None,
        };
        match tag {
            513 | 273 => offset = value,
            514 | 279 => length = value,
            _ => {}
        }
    }
    if let (Some(offset), Some(length)) = (offset, length)
        && let Some(thumbnail) = blob.get_mut(offset..offset.saturating_add(length))
    {
        thumbnail.fill(0);
    }
    if let Some(directory) = blob.get_mut(ifd1..ifd1 + 2 + 12 * entries + 4) {
        directory.fill(0);
    }
}

/// Pixels per unit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Density {
    pub x: f64,
    pub y: f64,
    pub unit: DensityUnit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DensityUnit {
    /// No unit: only the pixels' aspect ratio.
    Ratio,
    Inch,
    Centimeter,
}

const X_RESOLUTION: u16 = 282;
const Y_RESOLUTION: u16 = 283;
const RESOLUTION_UNIT: u16 = 296;

impl Density {
    /// From TIFF tags 282, 283 and 296 (whose default is inches).
    pub fn from_tags(tags: &TagDirectory) -> Option<Self> {
        let x = tags.get(X_RESOLUTION)?.rational()?;
        let y = tags.get(Y_RESOLUTION)?.rational()?;
        let unit = match tags.get(RESOLUTION_UNIT).and_then(TagValue::short) {
            Some(1) => DensityUnit::Ratio,
            Some(3) => DensityUnit::Centimeter,
            _ => DensityUnit::Inch,
        };
        (x > 0.0 && y > 0.0).then_some(Density { x, y, unit })
    }

    /// As TIFF tags 282, 283 and 296.
    pub fn to_tags(self) -> [(u16, TagValue); 3] {
        let unit = match self.unit {
            DensityUnit::Ratio => 1,
            DensityUnit::Inch => 2,
            DensityUnit::Centimeter => 3,
        };
        [
            (X_RESOLUTION, TagValue::from_rational(self.x)),
            (Y_RESOLUTION, TagValue::from_rational(self.y)),
            (RESOLUTION_UNIT, TagValue::from_short(unit)),
        ]
    }

    /// In whole pixels per unit, as JPEG and PNG store it.
    pub fn rounded(self, scale: f64) -> (u32, u32) {
        let round = |v: f64| (v * scale).round().clamp(1.0, f64::from(u32::MAX)) as u32;
        (round(self.x), round(self.y))
    }
}

/// Photoshop resources that hold a copy of the old picture: thumbnails (old and current
/// format), and EXIF copies (which can hold one).
const OLD_PICTURE: [u16; 4] = [0x0409, 0x040C, 0x0422, 0x0423];
/// The Photoshop resource holding IPTC-IIM records, and the one holding their digest.
const IPTC_RESOURCE: u16 = 0x0404;
const IPTC_DIGEST: u16 = 0x0425;

/// One Photoshop image resource: signature, id, Pascal name (with its padding) and data.
struct Resource<'a> {
    signature: &'a [u8],
    id: u16,
    name: &'a [u8],
    data: &'a [u8],
}

/// The resources of a Photoshop resource block, as far as they can be read.
fn resources(block: &[u8]) -> Vec<Resource<'_>> {
    let mut out = Vec::new();
    let mut at = 0;
    let mut next = || -> Option<Resource<'_>> {
        let signature = block.get(at..at + 4)?;
        if signature == [0; 4] {
            return None; // padding
        }
        let id = u16::from_be_bytes(block.get(at + 4..at + 6)?.try_into().ok()?);
        let name_len = usize::from(*block.get(at + 6)?);
        let name_end = at + 6 + (1 + name_len).next_multiple_of(2);
        let name = block.get(at + 6..name_end)?;
        let size = u32::from_be_bytes(block.get(name_end..name_end + 4)?.try_into().ok()?);
        let start = name_end + 4;
        let data = block.get(start..start.checked_add(size as usize)?)?;
        at = start + data.len().next_multiple_of(2);
        Some(Resource {
            signature,
            id,
            name,
            data,
        })
    };
    while let Some(resource) = next() {
        out.push(resource);
    }
    out
}

fn write_resources<'a>(list: impl IntoIterator<Item = Resource<'a>>) -> Vec<u8> {
    let mut out = Vec::new();
    for r in list {
        out.extend_from_slice(r.signature);
        out.extend_from_slice(&r.id.to_be_bytes());
        out.extend_from_slice(r.name);
        out.extend_from_slice(&(r.data.len() as u32).to_be_bytes());
        out.extend_from_slice(r.data);
        if r.data.len() % 2 == 1 {
            out.push(0);
        }
    }
    out
}

/// A Photoshop resource block without the copies of the old picture it holds, or `None` if
/// nothing readable is left.
pub(crate) fn photoshop_without_thumbnails(block: Vec<u8>) -> Option<Vec<u8>> {
    let list = resources(&block);
    if list.is_empty() {
        eprintln!("Photoshop resources left out: unreadable");
        return None;
    }
    Some(write_resources(
        list.into_iter().filter(|r| !OLD_PICTURE.contains(&r.id)),
    ))
    .filter(|b| !b.is_empty())
}

/// The IPTC-IIM records in a Photoshop resource block.
pub(crate) fn photoshop_iptc(block: &[u8]) -> Option<Vec<u8>> {
    resources(block)
        .into_iter()
        .find(|r| r.id == IPTC_RESOURCE)
        .map(|r| r.data.to_vec())
}

/// The Photoshop block for JPEG, where it carries IPTC: its own IPTC resource is kept (with
/// the digest that goes with it); one made from `iptc` is added only where it is missing, and
/// then without a digest, which would be stale.
pub(crate) fn photoshop_for_jpeg(block: Option<&[u8]>, iptc: Option<&[u8]>) -> Option<Vec<u8>> {
    let list = block.map(resources).unwrap_or_default();
    match iptc {
        Some(iptc) if !list.iter().any(|r| r.id == IPTC_RESOURCE) => {
            let mut list: Vec<_> = list.into_iter().filter(|r| r.id != IPTC_DIGEST).collect();
            list.push(Resource {
                signature: b"8BIM",
                id: IPTC_RESOURCE,
                name: &[0, 0],
                data: iptc,
            });
            Some(write_resources(list))
        }
        _ => block.map(<[u8]>::to_vec),
    }
}

/// `block` in parts of at most `limit` bytes, for JPEG segments: cut between resources where
/// possible (readers join the parts again).
pub(crate) fn photoshop_parts(block: &[u8], limit: usize) -> Vec<&[u8]> {
    let ends = resources(block).into_iter().map(|r| {
        let end = r.data.as_ptr() as usize - block.as_ptr() as usize + r.data.len();
        end.next_multiple_of(2).min(block.len())
    });
    let mut parts = Vec::new();
    let (mut start, mut last) = (0, 0);
    for end in ends.chain([block.len()]) {
        while end - start > limit {
            if last > start {
                parts.push(&block[start..last]);
                start = last;
            } else {
                parts.push(&block[start..start + limit]);
                start += limit;
                last = start;
            }
        }
        last = end;
    }
    if start < block.len() {
        parts.push(&block[start..]);
    }
    parts
}

/// The Photoshop block for PNG, and for a TIFF made from another format, where IPTC has a
/// place of its own: without its IPTC resource, so that readers don't find the records twice,
/// and without the digest that goes with it. `None` if nothing is left.
pub(crate) fn photoshop_without_iptc(block: &[u8]) -> Option<Vec<u8>> {
    let list = resources(block);
    if !list.iter().any(|r| r.id == IPTC_RESOURCE) {
        return Some(block.to_vec());
    }
    Some(write_resources(
        list.into_iter()
            .filter(|r| r.id != IPTC_RESOURCE && r.id != IPTC_DIGEST),
    ))
    .filter(|b| !b.is_empty())
}

/// IPTC-IIM records without the zeros some writers pad them with (to a multiple of 4 in TIFF):
/// up to the end of the last record that reads.
pub(crate) fn iptc_records(iptc: &[u8]) -> &[u8] {
    // The end of the record at `at`, if it reads. Always past `at`, so the walk ends.
    let end = |at: usize| -> Option<usize> {
        let header = iptc.get(at..at.checked_add(5)?)?;
        if header[0] != 0x1C {
            return None;
        }
        let len = usize::from(u16::from_be_bytes([header[3], header[4]]));
        let (start, len) = if len & 0x8000 == 0 {
            (at + 5, len)
        } else {
            // An extended dataset: the length's length (at most 4 bytes in IIM), then the
            // length.
            let n = len & 0x7FFF;
            if n > 4 {
                return None;
            }
            let bytes = iptc.get(at + 5..at + 5 + n)?;
            let len = bytes.iter().fold(0usize, |l, &b| (l << 8) | usize::from(b));
            (at + 5 + n, len)
        };
        start.checked_add(len).filter(|&end| end <= iptc.len())
    };
    let mut at = 0;
    while let Some(next) = end(at) {
        at = next;
    }
    if at == 0 { iptc } else { &iptc[..at] }
}

/// XMP as UTF-8 text, as PNG's iTXt and JPEG require; XMP in UTF-16 (allowed in TIFF) is
/// converted. `None` if it is neither.
pub(crate) fn xmp_text(xmp: &[u8]) -> Option<std::borrow::Cow<'_, str>> {
    if let Ok(text) = std::str::from_utf8(xmp) {
        return Some(text.into());
    }
    let units = |be: bool| {
        xmp.as_chunks::<2>().0.iter().map(move |&b| {
            if be {
                u16::from_be_bytes(b)
            } else {
                u16::from_le_bytes(b)
            }
        })
    };
    let text = match xmp {
        [0xFE, 0xFF, ..] => String::from_utf16(&units(true).skip(1).collect::<Vec<_>>()),
        [0xFF, 0xFE, ..] => String::from_utf16(&units(false).skip(1).collect::<Vec<_>>()),
        _ => return None,
    };
    text.ok().map(Into::into)
}

/// XMP that fits in `limit` bytes for JPEG: without the packet's whitespace padding, then
/// (the one edit allowed, as Adobe's and exiftool's writers do) without
/// photoshop:DocumentAncestors. `None` if it still doesn't fit.
pub(crate) fn xmp_within(text: &str, limit: usize) -> Option<String> {
    let mut text = text.to_owned();
    if text.len() > limit
        && let Some(meta_end) = text.find("</x:xmpmeta>").map(|i| i + "</x:xmpmeta>".len())
        && let Some(packet_end) = text[meta_end..].find("<?xpacket end").map(|i| meta_end + i)
        && text[meta_end..packet_end].trim().is_empty()
    {
        text.replace_range(meta_end..packet_end, "\n");
    }
    if text.len() > limit
        && let Some(start) = text.find("<photoshop:DocumentAncestors")
    {
        let close = "</photoshop:DocumentAncestors>";
        let end = match text[start..].find('>') {
            Some(i) if text[start..start + i].ends_with('/') => Some(start + i + 1),
            _ => text[start..].find(close).map(|i| start + i + close.len()),
        };
        if let Some(end) = end {
            text.replace_range(start..end, "");
        }
    }
    (text.len() <= limit).then_some(text)
}

/// A profile from ImageMagick's "Raw profile type …" PNG text: a newline, the profile's name,
/// its length, then its bytes in hex.
pub(crate) fn decode_raw_profile(text: &str) -> Option<Vec<u8>> {
    let mut lines = text.trim_start_matches(['\n', '\r']).lines();
    lines.next()?; // the name
    let len: usize = lines.next()?.trim().parse().ok()?;
    let digits: Vec<u8> = lines
        .flat_map(str::bytes)
        .filter_map(|b| (b as char).to_digit(16).map(|d| d as u8))
        .collect();
    let bytes: Vec<u8> = digits
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[high, low]| (high << 4) | low)
        .take(len)
        .collect();
    (bytes.len() == len).then_some(bytes)
}

/// The text of a "Raw profile type `name`" PNG chunk holding `data`.
pub(crate) fn encode_raw_profile(name: &str, data: &[u8]) -> String {
    let mut text = format!("\n{name}\n{:8}\n", data.len());
    for line in data.chunks(36) {
        for b in line {
            let _ = write!(text, "{b:02x}");
        }
        text.push('\n');
    }
    text
}

impl From<png::DecodingError> for Error {
    fn from(e: png::DecodingError) -> Self {
        Error::Png(e.to_string())
    }
}

impl From<png::EncodingError> for Error {
    fn from(e: png::EncodingError) -> Self {
        Error::Png(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A resource with an odd-length name and odd-length data, to check the padding.
    fn resource(id: u16, name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = b"8BIM".to_vec();
        out.extend_from_slice(&id.to_be_bytes());
        out.push(name.len() as u8);
        out.extend_from_slice(name);
        if (1 + name.len()) % 2 == 1 {
            out.push(0);
        }
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    #[test]
    fn photoshop_thumbnails_go_and_the_rest_stays() {
        let caption = resource(0x0404, b"", b"\x1c\x02\x78\x00\x03abc");
        let resolution = resource(0x03ED, b"res", &[1, 2, 3, 4, 5, 6, 7, 8]);
        let block = [
            caption.clone(),
            resource(0x040C, b"", &[0xFF, 0xD8, 0xFF]),
            resolution.clone(),
            resource(0x0409, b"thumb", &[9; 5]),
            resource(0x0422, b"", b"II*\0"),
        ]
        .concat();
        let stripped = photoshop_without_thumbnails(block).unwrap();
        assert_eq!(stripped, [caption.clone(), resolution.clone()].concat());
        assert_eq!(
            photoshop_iptc(&stripped).as_deref(),
            Some(&b"\x1c\x02\x78\x00\x03abc"[..])
        );
        // A cut-off last resource: what reads is kept.
        let cut = [caption.clone(), resolution[..9].to_vec()].concat();
        assert_eq!(photoshop_without_thumbnails(cut).unwrap(), caption);
        assert_eq!(
            photoshop_without_thumbnails(resource(0x040C, b"", &[1])),
            None
        );
        assert_eq!(photoshop_without_thumbnails(b"garbage".to_vec()), None);
    }

    #[test]
    fn iptc_lives_in_the_photoshop_block_only_in_jpeg() {
        let other = resource(0x03ED, b"", &[1, 2]);
        let digest = resource(0x0425, b"", &[0; 16]);
        let own = [other.clone(), resource(0x0404, b"", b"old"), digest.clone()].concat();
        assert_eq!(
            photoshop_for_jpeg(Some(&own), Some(b"new")),
            Some(own.clone()),
            "kept"
        );
        let without = [other.clone(), digest].concat();
        let added = photoshop_for_jpeg(Some(&without), Some(b"new")).unwrap();
        assert_eq!(
            added,
            [other.clone(), resource(0x0404, b"", b"new")].concat(),
            "no digest"
        );
        assert_eq!(
            photoshop_iptc(&photoshop_for_jpeg(None, Some(b"x")).unwrap()).as_deref(),
            Some(&b"x"[..])
        );
        assert_eq!(
            photoshop_without_iptc(&own),
            Some(other.clone()),
            "the digest goes with the records"
        );
        assert_eq!(photoshop_without_iptc(&other), Some(other));
        assert_eq!(photoshop_without_iptc(&resource(0x0404, b"", b"x")), None);
    }

    #[test]
    fn a_large_photoshop_block_is_cut_between_resources() {
        let (a, b, c) = (
            resource(1, b"", &[1; 40]),
            resource(2, b"", &[2; 30]),
            resource(3, b"", &[3; 100]),
        );
        let block = [a.clone(), b.clone(), c.clone()].concat();
        let parts = photoshop_parts(&block, 100);
        assert_eq!(parts[0], [a, b].concat());
        assert_eq!(parts[1..].concat(), c, "the one too large is cut anywhere");
        assert!(parts.iter().all(|p| p.len() <= 100));
        assert_eq!(photoshop_parts(&block, 1000), [&block[..]]);
    }

    #[test]
    fn iptc_padding_is_trimmed() {
        let records = b"\x1c\x01\x5a\x00\x03\x1b\x25\x47\x1c\x02\x78\x00\x02hi";
        let padded = [records.as_slice(), &[0, 0, 0]].concat();
        assert_eq!(iptc_records(&padded), records);
        assert_eq!(iptc_records(records), records);
        assert_eq!(iptc_records(b"\0\0junk"), b"\0\0junk", "unreadable: kept");
    }

    /// An extended length that would wrap the offset around (review-B #1) ends the walk.
    #[test]
    fn an_overflowing_iptc_length_ends_the_walk() {
        let records = b"\x1c\x02\x78\x00\x02hi";
        let wrapping = b"\x1c\x02\x78\x80\x08\xff\xff\xff\xff\xff\xff\xff\xf3";
        let iptc = [records.as_slice(), wrapping].concat();
        assert_eq!(iptc_records(&iptc), records);
        assert_eq!(iptc_records(wrapping), wrapping, "nothing reads: kept");
        // An extended length within IIM's 4 bytes still reads.
        let extended = [b"\x1c\x02\x78\x80\x02\x00\x03".as_slice(), b"abc"].concat();
        assert_eq!(
            iptc_records(&[extended.as_slice(), &[0, 0]].concat()),
            extended
        );
    }

    #[test]
    fn xmp_is_made_to_fit_a_jpeg_segment() {
        let ancestors = "<photoshop:DocumentAncestors><rdf:Bag><rdf:li>x</rdf:li></rdf:Bag></photoshop:DocumentAncestors>";
        let xmp = format!(
            "<?xpacket begin='' id='W5M0MpCehiHzreSzNTczkc9d'?><x:xmpmeta xmlns:x='adobe:ns:meta/'>{ancestors}<a/></x:xmpmeta>{}<?xpacket end='w'?>",
            " ".repeat(2000)
        );
        let small = xmp_within(&xmp, 1000).unwrap();
        assert!(
            small.contains(ancestors) && small.len() < 1000,
            "padding gone"
        );
        let smaller = xmp_within(&xmp, 150).unwrap();
        assert!(!smaller.contains("DocumentAncestors") && smaller.contains("<a/>"));
        assert_eq!(xmp_within(&xmp, 20), None);

        let utf16: Vec<u8> = [0xFEFF_u16]
            .into_iter()
            .chain("<x:xmpmeta/>".encode_utf16())
            .flat_map(u16::to_be_bytes)
            .collect();
        assert_eq!(xmp_text(&utf16).as_deref(), Some("<x:xmpmeta/>"));
        assert_eq!(xmp_text(&[0xC3, 0x28]), None);
    }

    #[test]
    fn raw_profiles_round_trip() {
        let data: Vec<u8> = (0..=255).collect();
        let text = encode_raw_profile("iptc", &data);
        assert!(text.starts_with("\niptc\n     256\n"));
        assert_eq!(decode_raw_profile(&text), Some(data));
        assert_eq!(decode_raw_profile("\niptc\n4\n0102"), None, "too short");
    }

    /// A little-endian EXIF blob: IFD0 with Orientation, IFD1 pointing at a 4-byte "thumbnail".
    fn blob_with_thumbnail() -> Vec<u8> {
        let mut b = b"II*\0\x08\0\0\0".to_vec();
        // IFD0 at 8: one entry, Orientation = 6, next IFD at 26.
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]);
        b.extend_from_slice(&26u32.to_le_bytes());
        // IFD1 at 26: offset 513 = 56, length 514 = 4.
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&[0x01, 0x02, 4, 0, 1, 0, 0, 0, 56, 0, 0, 0]);
        b.extend_from_slice(&[0x02, 0x02, 4, 0, 1, 0, 0, 0, 4, 0, 0, 0]);
        b.extend_from_slice(&0u32.to_le_bytes());
        // The thumbnail at 56.
        b.extend_from_slice(&[0xFF, 0xD8, 0xFF, 0xD9]);
        b
    }

    #[test]
    fn the_exif_thumbnail_is_unlinked_and_blanked_in_place() {
        let blob = blob_with_thumbnail();
        let Some(Exif::Blob(out)) = Exif::from_blob(blob.clone()) else {
            panic!("the EXIF was dropped")
        };
        assert_eq!(out.len(), blob.len(), "nothing moves");
        assert_eq!(out[..22], blob[..22], "IFD0 untouched");
        assert_eq!(out[22..26], [0, 0, 0, 0], "no next directory");
        assert!(
            out[26..].iter().all(|&b| b == 0),
            "IFD1 and thumbnail blanked"
        );
        assert_eq!(Exif::Blob(out).orientation(), 6);
    }

    /// Only a TIFF structure that reads is kept (review-B #7, test-B #5).
    #[test]
    fn exif_that_isnt_a_readable_tiff_is_dropped() {
        assert!(
            Exif::from_blob(b"<x:xmpmeta/>".to_vec()).is_none(),
            "not TIFF"
        );
        let mut lost = b"II*\0".to_vec();
        lost.extend_from_slice(&4000u32.to_le_bytes());
        lost.extend_from_slice(&[0; 20]);
        assert!(Exif::from_blob(lost).is_none(), "IFD0 past the end");
        assert!(Exif::from_blob(blob_with_thumbnail()).is_some());
    }

    #[test]
    fn density_converts_between_units() {
        let d = Density {
            x: 300.0,
            y: 150.0,
            unit: DensityUnit::Inch,
        };
        let tags = TagDirectory {
            tags: d.to_tags().to_vec(),
            children: Vec::new(),
        };
        assert_eq!(Density::from_tags(&tags), Some(d));
        assert_eq!(d.rounded(1.0 / 0.0254), (11811, 5906));
    }
}
